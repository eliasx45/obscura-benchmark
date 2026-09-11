//! WPT runner: runs classic testharness tests through `obscura fetch` and,
//! optionally, testharness plus screenshot reftests through Obscura's CDP.
//!
//! Work is divided into round-robin buckets. Fetch workers spawn isolated CLI
//! processes; CDP workers reuse one connection while giving every test its own
//! target. Console output and uncaught exceptions are retained for triage.

mod capture;
mod cdp;
mod fetch;
mod manifest;
mod reftest;
mod report;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use clap::{Parser, ValueEnum};
use serde_json::{json, Value};

use cdp::Cdp;
use manifest::{load_tests, TestCase, TestType, UrlBuilder};
use report::{print_results, summarize, Comparison, FileResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Profile {
    NoRender,
    Render,
}

impl Profile {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoRender => "no-render",
            Self::Render => "render",
        }
    }
}

#[derive(Parser)]
#[command(
    name = "wpt-runner",
    about = "Run the Web Platform Tests against Obscura over CDP."
)]
struct Args {
    /// Substring filter on the test path.
    filter: Option<String>,

    /// Obscura build profile. Render adds reftests to the classic testharness suite.
    #[arg(long, value_enum, default_value_t = Profile::NoRender)]
    profile: Profile,

    /// Execution backend. "fetch" runs one `obscura fetch` process per test;
    /// "cdp" drives a running `obscura serve` and is required for reftests.
    #[arg(long, default_value = "fetch")]
    backend: String,
    /// Path to the obscura binary used by the fetch backend.
    #[arg(long, default_value = "obscura")]
    obscura_bin: String,
    /// Seconds to let each page settle so the harness can finish (fetch backend,
    /// passed as `obscura fetch --wait`).
    #[arg(long, default_value_t = 2)]
    wait_secs: u64,

    /// Host the Obscura CDP endpoint listens on.
    #[arg(long, default_value = "127.0.0.1")]
    browser_host: String,
    /// Port the Obscura CDP endpoint listens on.
    #[arg(long, default_value_t = 9222)]
    browser_port: u16,
    /// Explicit browser WebSocket url. Skips discovery when set.
    #[arg(long)]
    ws_url: Option<String>,

    /// Host the WPT server is reachable as.
    #[arg(long, default_value = "web-platform.test")]
    wpt_host: String,
    /// WPT plain-HTTP port.
    #[arg(long, default_value_t = 8000)]
    http_port: u16,
    /// WPT HTTPS port.
    #[arg(long, default_value_t = 8443)]
    https_port: u16,
    /// WPT HTTP/2 port.
    #[arg(long, default_value_t = 9000)]
    h2_port: u16,

    /// Path to the WPT MANIFEST.json.
    #[arg(long, default_value = "./wpt/MANIFEST.json")]
    manifest: PathBuf,

    /// Include HTTPS tests. Set SSL_CERT_FILE to WPT's local CA certificate.
    #[arg(long, default_value_t = false)]
    include_https: bool,

    /// Number of parallel workers (each with its own CDP connection).
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
    /// Per-test timeout in milliseconds.
    #[arg(long, default_value_t = 15000)]
    test_timeout_ms: u64,
    /// Timeout for tests carrying WPT's `timeout=long` metadata.
    #[arg(long, default_value_t = 60000)]
    long_test_timeout_ms: u64,
    /// How often to poll for the test result, in milliseconds.
    #[arg(long, default_value_t = 50)]
    poll_interval_ms: u64,

    /// Print only the final summary, not per-file lines.
    #[arg(long, default_value_t = false)]
    summary: bool,
    /// Emit machine-readable JSON (the triage tool's input contract).
    #[arg(long, default_value_t = false)]
    json: bool,

    /// Run a single explicit url instead of loading the manifest.
    #[arg(long)]
    url: Option<String>,

    /// Exact WPT revision recorded in JSON output.
    #[arg(long, default_value = "unknown")]
    wpt_revision: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let use_fetch = args.backend.eq_ignore_ascii_case("fetch");
    if args.profile == Profile::Render && use_fetch {
        return Err(anyhow!("the render profile requires --backend cdp"));
    }

    // The CDP backend needs the browser WebSocket; the fetch backend spawns its
    // own processes and needs no running server.
    let ws_url = if use_fetch {
        String::new()
    } else {
        match &args.ws_url {
            Some(u) => u.clone(),
            None => cdp::discover_ws_url(&args.browser_host, args.browser_port).await?,
        }
    };

    // Build the work list. An explicit --url runs exactly as given (any
    // scheme); the https skip only applies to tests loaded from the manifest.
    let tests = if let Some(url) = &args.url {
        vec![TestCase::explicit(url)]
    } else {
        let base = UrlBuilder {
            host: args.wpt_host.clone(),
            http_port: args.http_port,
            https_port: args.https_port,
            h2_port: args.h2_port,
        };
        let mut tests = load_tests(
            &args.manifest,
            &base,
            args.filter.as_deref(),
            args.profile == Profile::Render,
        )?;
        if !args.include_https {
            let before = tests.len();
            tests.retain(|t| t.url.starts_with("http://"));
            let skipped = before - tests.len();
            if skipped > 0 {
                eprintln!("skipping {skipped} https test(s); pass --include-https to run them");
            }
        }
        tests
    };

    if tests.is_empty() {
        return Err(anyhow!(
            "no tests to run (check the filter, manifest, or --url)"
        ));
    }

    let concurrency = args.concurrency.max(1);
    if use_fetch {
        eprintln!(
            "running {} file(s) with {} worker(s) via `{} fetch`",
            tests.len(),
            concurrency,
            args.obscura_bin
        );
    } else {
        eprintln!(
            "running {} file(s) with {} worker(s) against {ws_url}",
            tests.len(),
            concurrency
        );
    }

    // Round-robin the tests into one bucket per worker so each worker gets a
    // roughly even mix regardless of how the manifest is ordered.
    let mut buckets: Vec<Vec<TestCase>> = (0..concurrency).map(|_| Vec::new()).collect();
    for (i, tc) in tests.into_iter().enumerate() {
        buckets[i % concurrency].push(tc);
    }

    let cfg = WorkerCfg {
        ws_url,
        obscura_bin: args.obscura_bin.clone(),
        test_timeout: Duration::from_millis(args.test_timeout_ms),
        long_test_timeout: Duration::from_millis(args.long_test_timeout_ms),
        poll_interval: Duration::from_millis(args.poll_interval_ms),
        wait_secs: args.wait_secs,
        use_fetch,
        verbose: !args.summary && !args.json,
    };

    let start = Instant::now();
    let mut handles = Vec::with_capacity(buckets.len());
    for bucket in buckets {
        if bucket.is_empty() {
            continue;
        }
        let cfg = cfg.clone();
        handles.push(tokio::spawn(async move { run_bucket(cfg, bucket).await }));
    }

    let mut all = Vec::new();
    for h in handles {
        match h.await {
            Ok(mut results) => all.append(&mut results),
            Err(e) => eprintln!("worker task panicked: {e}"),
        }
    }
    let elapsed = start.elapsed();

    all.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.url.cmp(&b.url)));
    let engine_version = std::process::Command::new(&args.obscura_bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "unknown".into());
    print_results(
        &all,
        elapsed,
        args.json,
        args.profile.as_str(),
        &engine_version,
        &args.wpt_revision,
    );

    let s = summarize(&all);
    if s.files_error > 0 || s.subtest_fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[derive(Clone)]
struct WorkerCfg {
    ws_url: String,
    obscura_bin: String,
    test_timeout: Duration,
    long_test_timeout: Duration,
    poll_interval: Duration,
    wait_secs: u64,
    use_fetch: bool,
    verbose: bool,
}

/// Run one worker's slice of the test list on a single CDP connection. If the
/// connection dies mid-bucket we reconnect once before the next test so a single
/// crash does not lose the rest of the bucket.
async fn run_bucket(cfg: WorkerCfg, bucket: Vec<TestCase>) -> Vec<FileResult> {
    if cfg.use_fetch {
        let mut results = Vec::with_capacity(bucket.len());
        for tc in &bucket {
            let result = if let Some(reason) = &tc.unsupported_reason {
                FileResult::unsupported(tc, reason.clone())
            } else if tc.test_type == TestType::Reftest {
                FileResult::unsupported(tc, "reftests require the CDP backend".into())
            } else {
                fetch::run_fetch(&cfg.obscura_bin, tc, timeout_for(&cfg, tc), cfg.wait_secs).await
            };
            if cfg.verbose {
                println!("{}", result.line());
            }
            results.push(result);
        }
        return results;
    }

    let mut results = Vec::with_capacity(bucket.len());
    let mut conn = Cdp::connect(&cfg.ws_url).await.ok();

    for tc in &bucket {
        if let Some(reason) = &tc.unsupported_reason {
            let result = FileResult::unsupported(tc, reason.clone());
            if cfg.verbose {
                println!("{}", result.line());
            }
            results.push(result);
            continue;
        }
        if conn.is_none() {
            conn = Cdp::connect(&cfg.ws_url).await.ok();
        }
        let mut conn_dead = false;
        let result = match conn.as_mut() {
            Some(c) if tc.test_type == TestType::Reftest => {
                run_reftest(c, &cfg, tc, &mut conn_dead).await
            }
            Some(c) => run_one(c, &cfg, tc, &mut conn_dead).await,
            None => Err(anyhow!("no CDP connection")),
        };

        let result = match result {
            Ok(r) => {
                // A timed-out CDP call leaves the socket mid-frame, so the next
                // test must start on a fresh connection.
                if conn_dead {
                    conn = None;
                }
                r
            }
            Err(e) => {
                conn = None;
                FileResult::runner_error(tc, e.to_string())
            }
        };

        if cfg.verbose {
            println!("{}", result.line());
        }
        results.push(result);
    }

    results
}

fn timeout_for(cfg: &WorkerCfg, test: &TestCase) -> Duration {
    if test.long_timeout {
        cfg.long_test_timeout
    } else {
        cfg.test_timeout
    }
}

/// Run a single test file end to end on the given connection.
///
/// Every CDP call is bounded by a timeout so a single hung call (a wedged
/// navigation or a test stuck in an infinite loop) cannot stall the worker.
/// On a hang we still drain the buffered events (the captured console output
/// and exceptions are the most useful bug signal) and set `conn_dead` so the
/// caller drops the now mid-frame connection before the next test.
async fn run_one(
    conn: &mut Cdp,
    cfg: &WorkerCfg,
    tc: &TestCase,
    conn_dead: &mut bool,
) -> Result<FileResult> {
    use tokio::time::timeout;
    let test_timeout = timeout_for(cfg, tc);
    // Control-plane calls (create/enable/close) get a short cap; navigation and
    // evaluation are bounded by the per-test budget.
    let ctrl_cap = test_timeout.min(Duration::from_secs(10));

    let created = match timeout(
        ctrl_cap,
        conn.call("Target.createTarget", json!({ "url": "about:blank" })),
    )
    .await
    {
        Ok(r) => r?,
        Err(_) => {
            *conn_dead = true;
            return Err(anyhow!("Target.createTarget timed out"));
        }
    };
    let target_id = created
        .pointer("/targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Target.createTarget returned no targetId"))?
        .to_string();
    let session = format!("{target_id}-session");

    // Best-effort: some builds may not implement every domain.
    let _ = timeout(ctrl_cap, conn.enable_capture(&session)).await;

    let start = Instant::now();
    let deadline = start + test_timeout;

    // Navigate within the per-test budget. A hang here means the page or the
    // connection is wedged: capture what we have and report a timeout.
    let nav_hung = timeout(
        test_timeout,
        conn.call_session(&session, "Page.navigate", json!({ "url": tc.url })),
    )
    .await
    .is_err();

    let mut payload: Option<Value> = None;
    if nav_hung {
        *conn_dead = true;
    } else {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match timeout(
                remaining,
                conn.call_session(
                    &session,
                    "Runtime.evaluate",
                    json!({ "expression": "window.__wptresults_json", "returnByValue": true }),
                ),
            )
            .await
            {
                Ok(Ok(evaluated)) => {
                    if let Some(s) = evaluated.pointer("/result/value").and_then(Value::as_str) {
                        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                            payload = Some(parsed);
                            break;
                        }
                    }
                }
                Ok(Err(_)) => {
                    // CDP-level error: the connection is suspect, stop polling.
                    *conn_dead = true;
                    break;
                }
                Err(_) => {
                    // Evaluate hung: the page is stuck.
                    *conn_dead = true;
                    break;
                }
            }
            tokio::time::sleep(cfg.poll_interval).await;
        }
    }

    // Drain buffered events into console/exception lines. `take_events` only
    // touches the buffered vec, so it is safe even after a cancelled call.
    // Obscura does not currently emit console/exception events over CDP, so the
    // report script also captures them in-page and ships them in the payload;
    // fold both sources together.
    let (mut console, mut exceptions) = capture::extract(&conn.take_events());
    if let Some(p) = &payload {
        for v in p
            .get("console")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(s) = v.as_str() {
                console.push(s.to_string());
            }
        }
        for v in p
            .get("errors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(s) = v.as_str() {
                exceptions.push(s.to_string());
            }
        }
    }
    let duration_ms = start.elapsed().as_millis() as u64;

    let mut result = match payload {
        Some(p) => FileResult::from_payload(tc, p),
        None => FileResult::timeout(tc),
    };
    result.console = console;
    result.exceptions = exceptions;
    result.duration_ms = duration_ms;

    // Best-effort cleanup, only while the connection still looks healthy.
    if !*conn_dead {
        let _ = timeout(
            ctrl_cap,
            conn.call("Target.closeTarget", json!({ "targetId": target_id })),
        )
        .await;
    }

    Ok(result)
}

async fn run_reftest(
    conn: &mut Cdp,
    cfg: &WorkerCfg,
    tc: &TestCase,
    conn_dead: &mut bool,
) -> Result<FileResult> {
    let start = Instant::now();
    let test_png = capture_png(conn, cfg, tc, &tc.url, conn_dead).await?;
    let mut comparisons = Vec::with_capacity(tc.references.len());
    for reference in &tc.references {
        if reference.relation != "==" && reference.relation != "!=" {
            return Err(anyhow!("unknown reftest relation `{}`", reference.relation));
        }
        let reference_png = capture_png(conn, cfg, tc, &reference.url, conn_dead).await?;
        let difference = reftest::compare_screenshots(&test_png, &reference_png, reference.fuzzy)?;
        let passed = if reference.relation == "==" {
            difference.equal
        } else {
            !difference.equal
        };
        comparisons.push(Comparison {
            reference: reference.url.clone(),
            relation: reference.relation.clone(),
            fuzzy: reference.fuzzy,
            equal: difference.equal,
            max_difference: difference.max_difference,
            different_pixels: difference.different_pixels,
            passed,
        });
    }

    let matches = comparisons
        .iter()
        .filter(|comparison| comparison.relation == "==");
    let has_match = matches.clone().next().is_some();
    let passed = (!has_match || matches.clone().any(|comparison| comparison.passed))
        && comparisons
            .iter()
            .filter(|comparison| comparison.relation == "!=")
            .all(|comparison| comparison.passed);
    let mut result = FileResult::reftest(tc, passed, comparisons);
    result.duration_ms = start.elapsed().as_millis() as u64;
    Ok(result)
}

async fn capture_png(
    conn: &mut Cdp,
    cfg: &WorkerCfg,
    tc: &TestCase,
    url: &str,
    conn_dead: &mut bool,
) -> Result<String> {
    use tokio::time::timeout;

    let budget = timeout_for(cfg, tc);
    let deadline = Instant::now() + budget;
    let ctrl_cap = budget.min(Duration::from_secs(10));
    let created = match timeout(
        ctrl_cap,
        conn.call("Target.createTarget", json!({ "url": "about:blank" })),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            *conn_dead = true;
            return Err(anyhow!("Target.createTarget timed out"));
        }
    };
    let target_id = created
        .pointer("/targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Target.createTarget returned no targetId"))?
        .to_string();
    let session = format!("{target_id}-session");
    let _ = timeout(ctrl_cap, conn.enable_capture(&session)).await;

    let remaining = deadline.saturating_duration_since(Instant::now());
    match timeout(
        remaining,
        conn.call_session(
            &session,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": 800,
                "height": 600,
                "deviceScaleFactor": 1,
                "mobile": false
            }),
        ),
    )
    .await
    {
        Ok(result) => {
            result?;
        }
        Err(_) => {
            *conn_dead = true;
            return Err(anyhow!("Emulation.setDeviceMetricsOverride timed out"));
        }
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    match timeout(
        remaining,
        conn.call_session(&session, "Page.navigate", json!({ "url": url })),
    )
    .await
    {
        Ok(result) => {
            result?;
        }
        Err(_) => {
            *conn_dead = true;
            return Err(anyhow!("Page.navigate timed out"));
        }
    }

    const READY: &str = r#"
        (async function () {
          if (document.readyState !== "complete") {
            await new Promise(function (resolve) {
              addEventListener("load", resolve, { once: true });
            });
          }
          if (document.fonts && document.fonts.ready) await document.fonts.ready;
          var root = document.documentElement;
          if (root && root.classList.contains("reftest-wait")) {
            root.dispatchEvent(new Event("TestRendered", { bubbles: true }));
            await new Promise(function (resolve) {
              var timer = setInterval(function () {
                if (!root.classList.contains("reftest-wait")) {
                  clearInterval(timer);
                  resolve();
                }
              }, 10);
            });
          }
          await new Promise(function (resolve) {
            if (typeof requestAnimationFrame !== "function") return setTimeout(resolve, 0);
            requestAnimationFrame(function () { requestAnimationFrame(resolve); });
          });
          return true;
        })()
    "#;
    let remaining = deadline.saturating_duration_since(Instant::now());
    let evaluated = match timeout(
        remaining,
        conn.call_session(
            &session,
            "Runtime.evaluate",
            json!({ "expression": READY, "awaitPromise": true, "returnByValue": true }),
        ),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            *conn_dead = true;
            return Err(anyhow!("reftest readiness timed out"));
        }
    };
    if let Some(exception) = evaluated.get("exceptionDetails") {
        return Err(anyhow!("reftest readiness failed: {exception}"));
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    let captured = match timeout(
        remaining,
        conn.call_session(
            &session,
            "Page.captureScreenshot",
            json!({ "format": "png", "fromSurface": true, "captureBeyondViewport": false }),
        ),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            *conn_dead = true;
            return Err(anyhow!("Page.captureScreenshot timed out"));
        }
    };
    let data = captured
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Page.captureScreenshot returned no data"))?
        .to_string();

    if !*conn_dead {
        let _ = timeout(
            ctrl_cap,
            conn.call("Target.closeTarget", json!({ "targetId": target_id })),
        )
        .await;
    }
    Ok(data)
}
