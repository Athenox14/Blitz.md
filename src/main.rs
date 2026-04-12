use anyhow::{bail, Context, Result};
use futures_util::{stream::FuturesUnordered, StreamExt};
use lol_html::{element, html_content::ContentType, HtmlRewriter, Settings};
use quick_xml::de::from_str;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use url::Url;

const DEFAULT_SEARCH_LIMIT: usize = 8;
const DEFAULT_TIMEOUT_MS: u64 = 900;
const DEFAULT_DEEP_LIMIT: usize = 3;
const SEARCH_RETRY_TIMEOUT_MS: u64 = 2500;

struct Sink<'a>(&'a mut String);

impl<'a> lol_html::OutputSink for Sink<'a> {
    fn handle_chunk(&mut self, chunk: &[u8]) {
        self.0.push_str(&String::from_utf8_lossy(chunk));
    }
}

#[derive(Debug)]
enum Command {
    Convert { url: String, verbose: bool },
    Search(SearchOptions),
}

#[derive(Debug, Clone)]
struct SearchOptions {
    query: String,
    limit: usize,
    timeout_ms: u64,
    json: bool,
    llm: bool,
    deep: bool,
    deep_limit: usize,
    verbose: bool,
}

#[derive(Debug, Clone, Serialize)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    source: String,
}

#[derive(Debug, Clone, Serialize)]
struct DeepResult {
    title: String,
    url: String,
    elapsed_ms: u128,
    markdown_excerpt: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SearchOutput {
    query: String,
    took_ms: u128,
    result_count: usize,
    results: Vec<SearchResult>,
    deep_results: Option<Vec<DeepResult>>,
    telemetry: Option<SearchTelemetry>,
    deep_telemetry: Option<DeepTelemetry>,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderMetric {
    name: String,
    elapsed_ms: u128,
    result_count: usize,
    status: String,
}

#[derive(Debug, Clone, Serialize)]
struct SearchTelemetry {
    total_elapsed_ms: u128,
    merge_elapsed_ms: u128,
    total_candidates: usize,
    unique_results: usize,
    providers: Vec<ProviderMetric>,
}

#[derive(Debug, Clone, Serialize)]
struct DeepTelemetry {
    requested: usize,
    completed: usize,
    succeeded: usize,
    failed: usize,
    total_elapsed_ms: u128,
    average_elapsed_ms: u128,
}

#[derive(Debug, Clone)]
struct SearchRun {
    results: Vec<SearchResult>,
    telemetry: SearchTelemetry,
}

#[derive(Debug, Deserialize, Default)]
struct RssFeed {
    #[serde(default)]
    channel: RssChannel,
}

#[derive(Debug, Deserialize, Default)]
struct RssChannel {
    #[serde(default)]
    item: Vec<RssItem>,
}

#[derive(Debug, Deserialize, Default)]
struct RssItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    description: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    let command = match parse_command(&args) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("{err}");
            print_usage(args.first().map_or("blitzmd", String::as_str));
            std::process::exit(1);
        }
    };

    let client = build_http_client()?;

    match command {
        Command::Convert { url, verbose } => {
            let markdown = convert_url_to_markdown(&client, &url, verbose).await?;
            println!("{markdown}");
        }
        Command::Search(options) => {
            run_search(&client, options).await?;
        }
    }

    Ok(())
}

fn parse_command(args: &[String]) -> Result<Command> {
    if args.len() <= 1 {
        bail!("Missing command arguments.");
    }

    let first = args[1].as_str();
    if first == "search" {
        return parse_search_command(args);
    }

    let verbose = args.iter().skip(1).any(|a| a == "--verbose" || a == "-v");
    let url = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .cloned()
        .context("Missing URL argument.")?;

    Ok(Command::Convert { url, verbose })
}

fn parse_search_command(args: &[String]) -> Result<Command> {
    if args.len() <= 2 {
        bail!("Missing search query.");
    }

    let mut limit = DEFAULT_SEARCH_LIMIT;
    let mut timeout_ms = DEFAULT_TIMEOUT_MS;
    let mut json = false;
    let mut llm = false;
    let mut deep = false;
    let mut deep_limit = DEFAULT_DEEP_LIMIT;
    let mut verbose = false;

    let mut query_parts: Vec<String> = Vec::new();
    let mut i = 2usize;

    while i < args.len() {
        match args[i].as_str() {
            "--limit" => {
                let Some(value) = args.get(i + 1) else {
                    bail!("--limit requires a value.");
                };
                limit = value
                    .parse::<usize>()
                    .with_context(|| format!("Invalid --limit value: {value}"))?;
                i += 2;
            }
            "--timeout-ms" => {
                let Some(value) = args.get(i + 1) else {
                    bail!("--timeout-ms requires a value.");
                };
                timeout_ms = value
                    .parse::<u64>()
                    .with_context(|| format!("Invalid --timeout-ms value: {value}"))?;
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--llm" => {
                llm = true;
                i += 1;
            }
            "--deep" => {
                deep = true;
                i += 1;
            }
            "--deep-limit" => {
                let Some(value) = args.get(i + 1) else {
                    bail!("--deep-limit requires a value.");
                };
                deep_limit = value
                    .parse::<usize>()
                    .with_context(|| format!("Invalid --deep-limit value: {value}"))?;
                i += 2;
            }
            "--verbose" | "-v" => {
                verbose = true;
                i += 1;
            }
            flag if flag.starts_with('-') => {
                bail!("Unknown search flag: {flag}");
            }
            term => {
                query_parts.push(term.to_string());
                i += 1;
            }
        }
    }

    if query_parts.is_empty() {
        bail!("Missing search query.");
    }

    if limit == 0 {
        bail!("--limit must be greater than zero.");
    }

    if deep_limit == 0 {
        bail!("--deep-limit must be greater than zero.");
    }

    let query = query_parts.join(" ");

    Ok(Command::Search(SearchOptions {
        query,
        limit,
        timeout_ms,
        json,
        llm,
        deep,
        deep_limit,
        verbose,
    }))
}

fn print_usage(bin: &str) {
    eprintln!("blitzmd - blazing-fast URL to Markdown converter + web search");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  {bin} [--verbose] <URL>");
    eprintln!(
        "  {bin} search [--limit N] [--timeout-ms MS] [--json] [--llm] [--deep] [--deep-limit N] [--verbose] <query>"
    );
}

fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .gzip(true)
        .brotli(true)
        .tcp_nodelay(true)
        .pool_max_idle_per_host(8)
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(12))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
        .build()
        .context("Failed to create HTTP client")
}

async fn run_search(client: &reqwest::Client, options: SearchOptions) -> Result<()> {
    let t0 = Instant::now();

    let mut search_run =
        search_web(client, &options.query, options.limit, options.timeout_ms, options.verbose).await;
    let mut results = search_run.results.clone();

    if results.is_empty() && should_retry_search(&search_run.telemetry) {
        let retry_timeout_ms = options.timeout_ms.max(SEARCH_RETRY_TIMEOUT_MS);
        if options.verbose {
            eprintln!(
                "[info] search fallback   : retrying with a wider timeout ({} ms)",
                retry_timeout_ms
            );
        }

        search_run =
            search_web(client, &options.query, options.limit, retry_timeout_ms, options.verbose).await;
        results = search_run.results.clone();
    }

    if results.is_empty() {
        bail!("No search results returned for query: {}", options.query);
    }

    let mut relevant = filter_and_rank_relevant_results(&results, &options.query, options.limit);
    if relevant.is_empty() {
        let retry_timeout_ms = options.timeout_ms.max(SEARCH_RETRY_TIMEOUT_MS);
        if options.verbose {
            eprintln!(
                "[warn] relevance filter : provider results look off-topic, retrying ({} ms)",
                retry_timeout_ms
            );
        }

        let retry_run =
            search_web(client, &options.query, options.limit, retry_timeout_ms, options.verbose).await;
        let retry_relevant =
            filter_and_rank_relevant_results(&retry_run.results, &options.query, options.limit);

        if !retry_relevant.is_empty() {
            search_run = retry_run;
            relevant = retry_relevant;
        }
    }

    if relevant.is_empty() {
        let quoted_query = format!("\"{}\"", options.query);
        if options.verbose {
            eprintln!("[info] relevance rescue : trying targeted DuckDuckGo query {quoted_query}");
        }
        if let Ok(extra_ddg) = search_duckduckgo(client, &quoted_query, options.limit).await {
            if !extra_ddg.is_empty() {
                results.extend(extra_ddg);
                let rescued =
                    filter_and_rank_relevant_results(&results, &options.query, options.limit);
                if !rescued.is_empty() {
                    relevant = rescued;
                }
            }
        }
    }

    if relevant.is_empty() {
        let discovered = discover_candidate_domains(client, &options.query, options.limit).await;
        if !discovered.is_empty() {
            if options.verbose {
                eprintln!(
                    "[info] relevance rescue : discovered {} candidate domain result(s)",
                    discovered.len()
                );
            }
            relevant = discovered;
        }
    }

    if relevant.is_empty() {
        bail!(
            "No relevant search results returned for query: {}",
            options.query
        );
    }
    results = relevant;

    if results.len() > options.limit {
        results.truncate(options.limit);
    }

    let deep_started = Instant::now();
    let deep_results = if options.deep {
        Some(enrich_results(
            client,
            &results,
            options.deep_limit.min(results.len()),
            options.verbose,
        )
        .await)
    } else {
        None
    };
    let deep_telemetry = deep_results
        .as_ref()
        .map(|deep| compute_deep_telemetry(deep, deep_started.elapsed()));

    if options.json {
        let payload = SearchOutput {
            query: options.query,
            took_ms: t0.elapsed().as_millis(),
            result_count: results.len(),
            results,
            deep_results,
            telemetry: Some(search_run.telemetry),
            deep_telemetry,
        };

        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if options.llm {
        print_llm_search_output(
            &options.query,
            &results,
            deep_results.as_deref(),
            &search_run.telemetry,
            deep_telemetry.as_ref(),
        );
    } else {
        println!("Query: {}", options.query);
        println!("Results: {}", results.len());
        println!();

        for (idx, result) in results.iter().enumerate() {
            println!("{}. {}", idx + 1, result.title);
            println!("   {}", result.url);
            if !result.snippet.is_empty() {
                println!("   {}", result.snippet);
            }
            println!("   source: {}", result.source);
            println!();
        }

        if let Some(deep) = deep_results.as_ref() {
            println!("Deep fetch ({} pages):", deep.len());
            println!();

            for item in deep {
                println!("- {}", item.title);
                println!("  {}", item.url);
                println!("  took: {} ms", item.elapsed_ms);

                if let Some(error) = item.error.as_ref() {
                    println!("  error: {error}");
                } else if let Some(excerpt) = item.markdown_excerpt.as_ref() {
                    println!("  excerpt:");
                    for line in excerpt.lines().take(20) {
                        println!("    {}", line);
                    }
                }
                println!();
            }
        }
    }

    if options.verbose {
        print_search_verbose(&search_run.telemetry, deep_telemetry.as_ref());
        eprintln!("[timing] search total    : {:.2?}", t0.elapsed());
    }

    Ok(())
}

async fn search_web(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    timeout_ms: u64,
    verbose: bool,
) -> SearchRun {
    let started = Instant::now();
    let timeout_duration = Duration::from_millis(timeout_ms);

    let ddg_started = Instant::now();
    let ddg_future = timeout(timeout_duration, search_duckduckgo(client, query, limit));
    tokio::pin!(ddg_future);

    let bing_started = Instant::now();
    let bing_future = timeout(timeout_duration, search_bing_rss(client, query, limit));
    tokio::pin!(bing_future);

    let mut providers: Vec<Vec<SearchResult>> = Vec::new();
    let mut provider_metrics = Vec::with_capacity(2);
    let mut ddg_done = false;
    let mut bing_done = false;

    loop {
        if ddg_done && bing_done {
            break;
        }

        tokio::select! {
            ddg_result = &mut ddg_future, if !ddg_done => {
                ddg_done = true;
                match ddg_result {
                    Ok(Ok(results)) => {
                        let elapsed_ms = ddg_started.elapsed().as_millis();
                        let is_empty = results.is_empty();
                        if verbose {
                            eprintln!(
                                "[timing] provider duckduckgo : {:.2?} ({} results)",
                                ddg_started.elapsed(),
                                results.len()
                            );
                        }
                        provider_metrics.push(ProviderMetric {
                            name: "duckduckgo".to_string(),
                            elapsed_ms,
                            result_count: results.len(),
                            status: if is_empty { "empty".to_string() } else { "ok".to_string() },
                        });
                        providers.push(results);
                    }
                    Ok(Err(err)) => {
                        provider_metrics.push(ProviderMetric {
                            name: "duckduckgo".to_string(),
                            elapsed_ms: ddg_started.elapsed().as_millis(),
                            result_count: 0,
                            status: format!("error: {err}"),
                        });
                        if verbose {
                            eprintln!("[warn] provider duckduckgo failed: {err}");
                        }
                    }
                    Err(_) => {
                        provider_metrics.push(ProviderMetric {
                            name: "duckduckgo".to_string(),
                            elapsed_ms: ddg_started.elapsed().as_millis(),
                            result_count: 0,
                            status: format!("timeout@{timeout_ms}ms"),
                        });
                        if verbose {
                            eprintln!("[warn] provider duckduckgo timed out after {timeout_ms}ms");
                        }
                    }
                }
            }
            bing_result = &mut bing_future, if !bing_done => {
                bing_done = true;
                match bing_result {
                    Ok(Ok(results)) => {
                        let elapsed_ms = bing_started.elapsed().as_millis();
                        let is_empty = results.is_empty();
                        if verbose {
                            eprintln!(
                                "[timing] provider bing-rss  : {:.2?} ({} results)",
                                bing_started.elapsed(),
                                results.len()
                            );
                        }
                        provider_metrics.push(ProviderMetric {
                            name: "bing-rss".to_string(),
                            elapsed_ms,
                            result_count: results.len(),
                            status: if is_empty { "empty".to_string() } else { "ok".to_string() },
                        });
                        providers.push(results);
                    }
                    Ok(Err(err)) => {
                        provider_metrics.push(ProviderMetric {
                            name: "bing-rss".to_string(),
                            elapsed_ms: bing_started.elapsed().as_millis(),
                            result_count: 0,
                            status: format!("error: {err}"),
                        });
                        if verbose {
                            eprintln!("[warn] provider bing-rss failed: {err}");
                        }
                    }
                    Err(_) => {
                        provider_metrics.push(ProviderMetric {
                            name: "bing-rss".to_string(),
                            elapsed_ms: bing_started.elapsed().as_millis(),
                            result_count: 0,
                            status: format!("timeout@{timeout_ms}ms"),
                        });
                        if verbose {
                            eprintln!("[warn] provider bing-rss timed out after {timeout_ms}ms");
                        }
                    }
                }
            }
        }

        if has_enough_unique_results(&providers, limit)
            && can_fastpath_with_current_providers(&provider_metrics, ddg_done, bing_done)
        {
            if verbose && !(ddg_done && bing_done) {
                eprintln!("[perf] fast path       : enough unique results reached, skipping wait");
            }
            if !ddg_done {
                provider_metrics.push(ProviderMetric {
                    name: "duckduckgo".to_string(),
                    elapsed_ms: ddg_started.elapsed().as_millis(),
                    result_count: 0,
                    status: "skipped-fastpath".to_string(),
                });
            }
            if !bing_done {
                provider_metrics.push(ProviderMetric {
                    name: "bing-rss".to_string(),
                    elapsed_ms: bing_started.elapsed().as_millis(),
                    result_count: 0,
                    status: "skipped-fastpath".to_string(),
                });
            }
            break;
        }
    }

    let total_candidates = providers.iter().map(std::vec::Vec::len).sum::<usize>();
    let merge_started = Instant::now();
    let results = merge_results_round_robin(providers, limit);
    let merge_elapsed_ms = merge_started.elapsed().as_millis();
    let unique_results = results.len();

    SearchRun {
        results,
        telemetry: SearchTelemetry {
            total_elapsed_ms: started.elapsed().as_millis(),
            merge_elapsed_ms,
            total_candidates,
            unique_results,
            providers: provider_metrics,
        },
    }
}

fn has_enough_unique_results(groups: &[Vec<SearchResult>], limit: usize) -> bool {
    if groups.is_empty() || limit == 0 {
        return false;
    }
    merge_results_round_robin(groups.to_vec(), limit).len() >= limit
}

fn can_fastpath_with_current_providers(
    metrics: &[ProviderMetric],
    ddg_done: bool,
    bing_done: bool,
) -> bool {
    // If both providers are done, no reason to keep waiting.
    if ddg_done && bing_done {
        return true;
    }

    // Avoid early return on Bing-only fast responses, which are sometimes noisy.
    metrics
        .iter()
        .any(|m| m.name == "duckduckgo" && m.status == "ok" && m.result_count > 0)
}

async fn search_duckduckgo(client: &reqwest::Client, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    let encoded = urlencoding::encode(query);
    let endpoint = format!("https://html.duckduckgo.com/html/?q={encoded}");

    let response = client
        .get(&endpoint)
        .send()
        .await
        .context("DuckDuckGo request failed")?;

    if !response.status().is_success() {
        bail!("DuckDuckGo returned HTTP {}", response.status());
    }

    let html = response.text().await.context("DuckDuckGo body read failed")?;
    let parsed = parse_duckduckgo_html(&html, limit);
    if !parsed.is_empty() {
        return Ok(parsed);
    }

    let lite_endpoint = format!("https://lite.duckduckgo.com/lite/?q={encoded}");
    let lite_response = client
        .get(&lite_endpoint)
        .send()
        .await
        .context("DuckDuckGo lite request failed")?;

    if !lite_response.status().is_success() {
        return Ok(parsed);
    }

    let lite_html = lite_response
        .text()
        .await
        .context("DuckDuckGo lite body read failed")?;
    let lite_parsed = parse_duckduckgo_html(&lite_html, limit);
    if lite_parsed.is_empty() {
        return Ok(parsed);
    }
    Ok(lite_parsed)
}

async fn search_bing_rss(client: &reqwest::Client, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    let mut primary = search_bing_rss_once(client, query, limit).await?;
    if !primary.is_empty() && best_relevance_score(&primary, query) > 0 {
        return Ok(primary);
    }

    let quoted = format!("\"{}\"", query);
    let quoted_results = search_bing_rss_once(client, &quoted, limit).await?;
    if best_relevance_score(&quoted_results, query) > best_relevance_score(&primary, query) {
        return Ok(quoted_results);
    }

    if primary.is_empty() {
        return Ok(quoted_results);
    }
    Ok(std::mem::take(&mut primary))
}

async fn search_bing_rss_once(client: &reqwest::Client, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    let encoded = urlencoding::encode(query);
    let endpoint = format!("https://www.bing.com/search?format=rss&q={encoded}");

    let response = client
        .get(&endpoint)
        .send()
        .await
        .context("Bing RSS request failed")?;

    if !response.status().is_success() {
        bail!("Bing RSS returned HTTP {}", response.status());
    }

    let xml = response.text().await.context("Bing RSS body read failed")?;
    parse_bing_rss(&xml, limit)
}

fn parse_duckduckgo_html(html: &str, limit: usize) -> Vec<SearchResult> {
    let document = Html::parse_document(html);

    let card_selector = Selector::parse(".result").expect("selector should be valid");
    let link_selector = Selector::parse(".result__a").expect("selector should be valid");
    let snippet_selector = Selector::parse(".result__snippet").expect("selector should be valid");
    let alt_link_selector =
        Selector::parse("a[data-testid='result-title-a']").expect("selector should be valid");
    let heading_link_selector = Selector::parse("h2 a").expect("selector should be valid");
    let any_link_selector = Selector::parse("a").expect("selector should be valid");

    let mut results = Vec::with_capacity(limit);
    let mut seen = HashSet::new();

    for card in document.select(&card_selector) {
        let Some(link) = card.select(&link_selector).next() else {
            continue;
        };

        let Some(href) = link.value().attr("href") else {
            continue;
        };

        let title = normalize_whitespace(&link.text().collect::<String>());
        if title.is_empty() {
            continue;
        }

        let snippet = card
            .select(&snippet_selector)
            .next()
            .map(|node| normalize_whitespace(&node.text().collect::<String>()))
            .unwrap_or_default();

        let url = normalize_duckduckgo_url(href);
        let dedupe = canonicalize_for_dedupe(&url);
        if !seen.insert(dedupe) {
            continue;
        }

        results.push(SearchResult {
            title,
            url,
            snippet,
            source: "duckduckgo".to_string(),
        });

        if results.len() == limit {
            break;
        }
    }

    if !results.is_empty() {
        return results;
    }

    for link in document.select(&link_selector).chain(document.select(&alt_link_selector)) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };

        let title = normalize_whitespace(&link.text().collect::<String>());
        if title.is_empty() {
            continue;
        }

        let url = normalize_duckduckgo_url(href);
        let dedupe = canonicalize_for_dedupe(&url);
        if !seen.insert(dedupe) {
            continue;
        }

        results.push(SearchResult {
            title,
            url,
            snippet: String::new(),
            source: "duckduckgo".to_string(),
        });

        if results.len() == limit {
            break;
        }
    }

    if !results.is_empty() {
        return results;
    }

    for link in document.select(&heading_link_selector).chain(document.select(&any_link_selector)) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };

        let url = normalize_duckduckgo_url(href);
        if !is_plausible_external_result_url(&url) {
            continue;
        }

        let title = normalize_whitespace(&link.text().collect::<String>());
        if title.len() < 8 {
            continue;
        }

        let dedupe = canonicalize_for_dedupe(&url);
        if !seen.insert(dedupe) {
            continue;
        }

        results.push(SearchResult {
            title,
            url,
            snippet: String::new(),
            source: "duckduckgo".to_string(),
        });

        if results.len() == limit {
            break;
        }
    }

    results
}

fn is_plausible_external_result_url(url: &str) -> bool {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }

    if let Ok(parsed) = Url::parse(url) {
        let host = parsed.host_str().unwrap_or_default().to_lowercase();
        return !host.is_empty()
            && host != "duckduckgo.com"
            && host != "html.duckduckgo.com"
            && host != "duck.co";
    }

    false
}

fn normalize_duckduckgo_url(raw_href: &str) -> String {
    if raw_href.starts_with("https://duckduckgo.com/l/?")
        || raw_href.starts_with("http://duckduckgo.com/l/?")
    {
        if let Ok(parsed) = Url::parse(raw_href) {
            for (key, value) in parsed.query_pairs() {
                if key == "uddg" {
                    return value.into_owned();
                }
            }
        }
    }

    if raw_href.starts_with("/l/?") {
        if let Ok(parsed) = Url::parse(&format!("https://duckduckgo.com{raw_href}")) {
            for (key, value) in parsed.query_pairs() {
                if key == "uddg" {
                    return value.into_owned();
                }
            }
        }
    }

    if raw_href.starts_with("//") {
        return format!("https:{raw_href}");
    }

    if raw_href.starts_with("http://") || raw_href.starts_with("https://") {
        return raw_href.to_string();
    }

    format!("https://duckduckgo.com{raw_href}")
}

fn parse_bing_rss(xml: &str, limit: usize) -> Result<Vec<SearchResult>> {
    let feed: RssFeed = from_str(xml).context("Unable to parse Bing RSS XML")?;

    let mut results = Vec::with_capacity(limit);

    for item in feed.channel.item.into_iter() {
        if item.link.trim().is_empty() || item.title.trim().is_empty() {
            continue;
        }

        let snippet = normalize_whitespace(&strip_and_decode(&item.description));

        results.push(SearchResult {
            title: normalize_whitespace(&item.title),
            url: item.link,
            snippet,
            source: "bing-rss".to_string(),
        });

        if results.len() == limit {
            break;
        }
    }

    Ok(results)
}

fn merge_results_round_robin(groups: Vec<Vec<SearchResult>>, limit: usize) -> Vec<SearchResult> {
    if groups.is_empty() || limit == 0 {
        return Vec::new();
    }

    let mut merged = Vec::with_capacity(limit);
    let mut seen = HashSet::new();

    let max_len = groups.iter().map(std::vec::Vec::len).max().unwrap_or(0);

    for row in 0..max_len {
        for group in &groups {
            let Some(item) = group.get(row) else {
                continue;
            };

            let dedupe_key = canonicalize_for_dedupe(&item.url);
            if !seen.insert(dedupe_key) {
                continue;
            }

            merged.push(item.clone());
            if merged.len() == limit {
                return merged;
            }
        }
    }

    merged
}

fn canonicalize_for_dedupe(value: &str) -> String {
    if let Ok(mut parsed) = Url::parse(value) {
        parsed.set_fragment(None);

        let default_port = parsed.port_or_known_default();
        if (parsed.scheme() == "http" && default_port == Some(80))
            || (parsed.scheme() == "https" && default_port == Some(443))
        {
            let _ = parsed.set_port(None);
        }

        let mut canonical = parsed.to_string();
        if parsed.query().is_none() && canonical.ends_with('/') {
            canonical.pop();
        }

        return canonical;
    }

    value.trim().to_lowercase()
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|s| s.len() >= 3)
        .map(|s| s.to_lowercase())
        .collect()
}

fn relevance_score(result: &SearchResult, terms: &[String]) -> usize {
    if terms.is_empty() {
        return 0;
    }

    let title = result.title.to_lowercase();
    let snippet = result.snippet.to_lowercase();
    let url = result.url.to_lowercase();

    terms
        .iter()
        .filter(|t| title.contains(t.as_str()) || snippet.contains(t.as_str()) || url.contains(t.as_str()))
        .count()
}

fn filter_and_rank_relevant_results(
    results: &[SearchResult],
    query: &str,
    limit: usize,
) -> Vec<SearchResult> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Vec::new();
    }

    let min_match = min_term_match_threshold(terms.len());
    let mut scored: Vec<(usize, SearchResult)> = results
        .iter()
        .cloned()
        .map(|r| (relevance_score(&r, &terms), r))
        .filter(|(score, _)| *score >= min_match)
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.truncate(limit);
    scored.into_iter().map(|(_, r)| r).collect()
}

fn best_relevance_score(results: &[SearchResult], query: &str) -> usize {
    let terms = query_terms(query);
    results
        .iter()
        .map(|r| relevance_score(r, &terms))
        .max()
        .unwrap_or(0)
}

fn min_term_match_threshold(term_count: usize) -> usize {
    match term_count {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 2,
    }
}

async fn discover_candidate_domains(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
) -> Vec<SearchResult> {
    let terms = query_terms(query);
    let Some(primary) = terms.first() else {
        return Vec::new();
    };
    if primary.len() < 4 {
        return Vec::new();
    }

    let tld_candidates = [".dev", ".com", ".io", ".fr"];
    let mut jobs = FuturesUnordered::new();

    for tld in tld_candidates {
        let host = format!("{primary}{tld}");
        let url = format!("https://{host}/");
        let client = client.clone();
        let primary = primary.clone();

        jobs.push(async move {
            let resp = client.get(&url).send().await.ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let body = resp.text().await.ok()?;
            let lower = body.to_lowercase();
            if !lower.contains(&primary) {
                return None;
            }

            let title = extract_html_title(&body).unwrap_or_else(|| host.clone());
            Some(SearchResult {
                title: normalize_whitespace(&title),
                url,
                snippet: "Discovered via domain fallback".to_string(),
                source: "heuristic-domain".to_string(),
            })
        });
    }

    let mut out = Vec::new();
    while let Some(item) = jobs.next().await {
        if let Some(result) = item {
            out.push(result);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title>")?;
    let end = lower[start + 7..].find("</title>")?;
    let raw = &html[start + 7..start + 7 + end];
    let title = normalize_whitespace(raw);
    if title.is_empty() {
        return None;
    }
    Some(title)
}

async fn enrich_results(
    client: &reqwest::Client,
    results: &[SearchResult],
    deep_limit: usize,
    verbose: bool,
) -> Vec<DeepResult> {
    let mut jobs = FuturesUnordered::new();

    for result in results.iter().take(deep_limit).cloned() {
        let cloned_client = client.clone();

        jobs.push(async move {
            let started = Instant::now();
            let markdown = convert_url_to_markdown(&cloned_client, &result.url, false).await;
            let elapsed = started.elapsed().as_millis();

            match markdown {
                Ok(content) => DeepResult {
                    title: result.title,
                    url: result.url,
                    elapsed_ms: elapsed,
                    markdown_excerpt: Some(markdown_excerpt(&content, 2200)),
                    error: None,
                },
                Err(err) => DeepResult {
                    title: result.title,
                    url: result.url,
                    elapsed_ms: elapsed,
                    markdown_excerpt: None,
                    error: Some(err.to_string()),
                },
            }
        });
    }

    let mut deep = Vec::with_capacity(deep_limit);
    while let Some(item) = jobs.next().await {
        if verbose {
            eprintln!("[timing] deep fetch       : {} ms -> {}", item.elapsed_ms, item.url);
        }
        deep.push(item);
    }

    deep
}

fn compute_deep_telemetry(deep: &[DeepResult], total_elapsed: Duration) -> DeepTelemetry {
    let succeeded = deep.iter().filter(|item| item.error.is_none()).count();
    let failed = deep.len().saturating_sub(succeeded);
    let average_elapsed_ms = if deep.is_empty() {
        0
    } else {
        deep.iter().map(|item| item.elapsed_ms).sum::<u128>() / deep.len() as u128
    };

    DeepTelemetry {
        requested: deep.len(),
        completed: deep.len(),
        succeeded,
        failed,
        total_elapsed_ms: total_elapsed.as_millis(),
        average_elapsed_ms,
    }
}

fn print_search_verbose(search: &SearchTelemetry, deep: Option<&DeepTelemetry>) {
    let search_seconds = duration_secs(search.total_elapsed_ms);
    let results_per_second = if search_seconds > 0.0 {
        search.unique_results as f64 / search_seconds
    } else {
        0.0
    };

    eprintln!(
        "[perf] merged results   : {} unique from {} candidates",
        search.unique_results, search.total_candidates
    );
    eprintln!(
        "[perf] search throughput: {:.2} results/s",
        results_per_second
    );
    eprintln!(
        "[timing] merge          : {} ms",
        search.merge_elapsed_ms
    );

    for provider in &search.providers {
        eprintln!(
            "[verbose] provider      : {} | status={} | {} ms | {} results",
            provider.name, provider.status, provider.elapsed_ms, provider.result_count
        );
    }

    if let Some(deep) = deep {
        let deep_seconds = duration_secs(deep.total_elapsed_ms);
        let pages_per_second = if deep_seconds > 0.0 {
            deep.completed as f64 / deep_seconds
        } else {
            0.0
        };

        eprintln!(
            "[perf] deep throughput  : {:.2} pages/s",
            pages_per_second
        );
        eprintln!(
            "[timing] deep total     : {} ms (avg {} ms/page, ok={}, failed={})",
            deep.total_elapsed_ms, deep.average_elapsed_ms, deep.succeeded, deep.failed
        );
    }
}

fn print_llm_search_output(
    query: &str,
    results: &[SearchResult],
    deep_results: Option<&[DeepResult]>,
    telemetry: &SearchTelemetry,
    deep_telemetry: Option<&DeepTelemetry>,
) {
    println!("# Search Results");
    println!("Query: {query}");
    println!("Returned: {}", results.len());
    println!("Search time: {} ms", telemetry.total_elapsed_ms);
    println!();

    for (idx, result) in results.iter().enumerate() {
        println!("## Result {}", idx + 1);
        println!("Title: {}", result.title);
        println!("URL: {}", result.url);
        println!("Source: {}", result.source);
        if !result.snippet.is_empty() {
            println!("Snippet: {}", result.snippet);
        }
        println!();
    }

    if let Some(deep) = deep_results {
        println!("# Deep Excerpts");
        println!();

        for (idx, item) in deep.iter().enumerate() {
            println!("## Page {}", idx + 1);
            println!("Title: {}", item.title);
            println!("URL: {}", item.url);
            println!("Elapsed: {} ms", item.elapsed_ms);
            if let Some(error) = item.error.as_ref() {
                println!("Error: {error}");
            } else if let Some(excerpt) = item.markdown_excerpt.as_ref() {
                println!("Excerpt:");
                println!("{excerpt}");
            }
            println!();
        }
    }

    println!("# Telemetry");
    println!("Candidates: {}", telemetry.total_candidates);
    println!("Unique results: {}", telemetry.unique_results);
    println!("Merge time: {} ms", telemetry.merge_elapsed_ms);
    for provider in &telemetry.providers {
        println!(
            "Provider: {} | status={} | {} ms | {} results",
            provider.name, provider.status, provider.elapsed_ms, provider.result_count
        );
    }
    if let Some(deep) = deep_telemetry {
        println!(
            "Deep fetch: {} pages | ok={} | failed={} | total={} ms | avg={} ms/page",
            deep.completed, deep.succeeded, deep.failed, deep.total_elapsed_ms, deep.average_elapsed_ms
        );
    }
}

fn duration_secs(ms: u128) -> f64 {
    ms as f64 / 1000.0
}

fn should_retry_search(telemetry: &SearchTelemetry) -> bool {
    telemetry.unique_results == 0
        && !telemetry.providers.is_empty()
        && telemetry
            .providers
            .iter()
            .all(|provider| provider.status != "ok")
}

fn markdown_excerpt(markdown: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(max_chars.min(markdown.len()));
    let mut count = 0usize;

    for ch in markdown.chars() {
        let width = ch.len_utf8();
        if count + width > max_chars {
            break;
        }
        out.push(ch);
        count += width;
    }

    out.trim().to_string()
}

async fn convert_url_to_markdown(client: &reqwest::Client, url: &str, verbose: bool) -> Result<String> {
    let t0 = Instant::now();

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to fetch: {url}"))?;

    if !response.status().is_success() {
        bail!("HTTP error: {}", response.status());
    }

    if verbose {
        eprintln!("[timing] connect+headers : {:.2?}", t0.elapsed());
    }

    let content_length = response.content_length().unwrap_or(64 * 1024) as usize;
    let mut body_stream = response.bytes_stream();
    let mut raw = String::with_capacity(content_length.max(8 * 1024));
    let mut downloaded_bytes = 0usize;
    let mut chunk_count = 0usize;

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("script", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("style", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("head", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("nav", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("footer", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("iframe", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("noscript", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("svg", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("input", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("select", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("textarea", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("button", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("h1", |el| {
                    el.before("\n\n# ", ContentType::Text);
                    el.after("\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("h2", |el| {
                    el.before("\n\n## ", ContentType::Text);
                    el.after("\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("h3", |el| {
                    el.before("\n\n### ", ContentType::Text);
                    el.after("\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("h4", |el| {
                    el.before("\n\n#### ", ContentType::Text);
                    el.after("\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("h5", |el| {
                    el.before("\n\n##### ", ContentType::Text);
                    el.after("\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("h6", |el| {
                    el.before("\n\n###### ", ContentType::Text);
                    el.after("\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("p", |el| {
                    el.before("\n\n", ContentType::Text);
                    el.after("\n\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("blockquote", |el| {
                    el.before("\n\n> ", ContentType::Text);
                    el.after("\n\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("pre", |el| {
                    el.before("\n\n```\n", ContentType::Text);
                    el.after("\n```\n\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("ul", |el| {
                    el.before("\n", ContentType::Text);
                    el.after("\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("ol", |el| {
                    el.before("\n", ContentType::Text);
                    el.after("\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("li", |el| {
                    el.before("\n* ", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("strong", |el| {
                    el.before("**", ContentType::Text);
                    el.after("**", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("b", |el| {
                    el.before("**", ContentType::Text);
                    el.after("**", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("em", |el| {
                    el.before("_", ContentType::Text);
                    el.after("_", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("i", |el| {
                    el.before("_", ContentType::Text);
                    el.after("_", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("code", |el| {
                    el.before("`", ContentType::Text);
                    el.after("`", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("a", |el| {
                    let href = el.get_attribute("href").unwrap_or_default();
                    if href.is_empty() || href.starts_with('#') {
                        el.remove_and_keep_content();
                    } else {
                        let abs = resolve_url(url, &href);
                        el.before("[", ContentType::Text);
                        el.after(&format!("]({abs})"), ContentType::Text);
                        el.remove_and_keep_content();
                    }
                    Ok(())
                }),
                element!("img", |el| {
                    let src = el.get_attribute("src").unwrap_or_default();
                    if !src.is_empty() {
                        let alt = el.get_attribute("alt").unwrap_or_default();
                        let resolved = resolve_url(url, &src);
                        el.before(&format!("![{alt}]({resolved})"), ContentType::Text);
                    }
                    el.remove();
                    Ok(())
                }),
                element!("br", |el| {
                    el.before("\n", ContentType::Text);
                    el.remove();
                    Ok(())
                }),
                element!("hr", |el| {
                    el.before("\n\n---\n\n", ContentType::Text);
                    el.remove();
                    Ok(())
                }),
                element!("table", |el| {
                    el.before("\n\n", ContentType::Text);
                    el.after("\n\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("tr", |el| {
                    el.before("\n", ContentType::Text);
                    el.after(" |", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("th", |el| {
                    el.before("| **", ContentType::Text);
                    el.after("** ", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("td", |el| {
                    el.before("| ", ContentType::Text);
                    el.after(" ", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("thead", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("tbody", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("tfoot", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("dl", |el| {
                    el.before("\n\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("dt", |el| {
                    el.before("\n**", ContentType::Text);
                    el.after("**", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("dd", |el| {
                    el.before("\n: ", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("figure", |el| {
                    el.before("\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("figcaption", |el| {
                    el.before("\n_", ContentType::Text);
                    el.after("_\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("html", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("body", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("main", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("header", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("aside", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("span", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("label", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("form", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("fieldset", |el| {
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("div", |el| {
                    el.before("\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("section", |el| {
                    el.before("\n\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
                element!("article", |el| {
                    el.before("\n\n", ContentType::Text);
                    el.remove_and_keep_content();
                    Ok(())
                }),
            ],
            ..Default::default()
        },
        Sink(&mut raw),
    );

    let t_body = Instant::now();
    while let Some(chunk) = body_stream.next().await {
        let chunk = chunk.context("Stream read error")?;
        downloaded_bytes += chunk.len();
        chunk_count += 1;
        rewriter
            .write(&chunk)
            .context("HTML parse error")?;
    }
    rewriter.end().context("HTML finalization error")?;
    let body_elapsed = t_body.elapsed();

    if verbose {
        let throughput_kb_s = if body_elapsed.as_secs_f64() > 0.0 {
            (downloaded_bytes as f64 / 1024.0) / body_elapsed.as_secs_f64()
        } else {
            0.0
        };
        eprintln!("[timing] body+parse      : {:.2?}", body_elapsed);
        eprintln!("[timing] raw size        : {} KB", raw.len() / 1024);
        eprintln!("[timing] download size   : {} KB", downloaded_bytes / 1024);
        eprintln!("[timing] stream chunks   : {}", chunk_count);
        eprintln!("[perf]   download speed  : {:.1} KB/s", throughput_kb_s);
    }

    let t_post = Instant::now();
    let result = post_process(&raw);
    let post_elapsed = t_post.elapsed();

    if verbose {
        let output_kb = result.len() / 1024;
        eprintln!("[timing] post_process    : {:.2?}", post_elapsed);
        eprintln!("[timing] output size     : {} KB", output_kb);
        eprintln!("[timing] total           : {:.2?}", t0.elapsed());
    }

    Ok(result)
}

fn resolve_url(base: &str, href: &str) -> String {
    if href.starts_with("//") {
        let scheme = Url::parse(base)
            .map(|u| u.scheme().to_string())
            .unwrap_or_else(|_| "https".to_string());
        return format!("{scheme}:{href}");
    }

    if let Ok(base_url) = Url::parse(base) {
        if let Ok(joined) = base_url.join(href) {
            return joined.to_string();
        }
    }

    href.to_string()
}

fn strip_and_decode(input: &str) -> String {
    let src = input.as_bytes();
    let mut dst: Vec<u8> = Vec::with_capacity(input.len());
    let mut i = 0;
    let mut in_tag = false;

    while i < src.len() {
        if in_tag {
            match src[i..].iter().position(|&b| b == b'>') {
                Some(pos) => {
                    i += pos + 1;
                    in_tag = false;
                }
                None => break,
            }
        } else {
            let start = i;
            while i < src.len() && src[i] != b'<' && src[i] != b'&' {
                i += 1;
            }
            dst.extend_from_slice(&src[start..i]);

            if i >= src.len() {
                break;
            }

            match src[i] {
                b'<' => {
                    in_tag = true;
                    i += 1;
                }
                b'&' => {
                    i += 1;
                    let name_start = i;
                    let semi = src[i..].iter().take(12).position(|&b| b == b';');
                    if let Some(len) = semi {
                        let entity = &src[i..i + len];
                        i += len + 1;

                        let replacement: Option<&[u8]> = match entity {
                            b"amp" => Some(b"&"),
                            b"lt" => Some(b"<"),
                            b"gt" => Some(b">"),
                            b"quot" => Some(b"\""),
                            b"apos" => Some(b"'"),
                            b"nbsp" => Some(b" "),
                            b"mdash" => Some("\u{2014}".as_bytes()),
                            b"ndash" => Some("\u{2013}".as_bytes()),
                            b"hellip" => Some("\u{2026}".as_bytes()),
                            b"laquo" => Some("\u{00AB}".as_bytes()),
                            b"raquo" => Some("\u{00BB}".as_bytes()),
                            b"copy" => Some("\u{00A9}".as_bytes()),
                            b"reg" => Some("\u{00AE}".as_bytes()),
                            b"trade" => Some("\u{2122}".as_bytes()),
                            b"euro" => Some("\u{20AC}".as_bytes()),
                            b"eacute" => Some("\u{00E9}".as_bytes()),
                            b"egrave" => Some("\u{00E8}".as_bytes()),
                            b"ecirc" => Some("\u{00EA}".as_bytes()),
                            b"agrave" => Some("\u{00E0}".as_bytes()),
                            b"ugrave" => Some("\u{00F9}".as_bytes()),
                            b"ccedil" => Some("\u{00E7}".as_bytes()),
                            e if e.first() == Some(&b'#') => {
                                let (hex, digits) = match e.get(1).copied() {
                                    Some(b'x') | Some(b'X') => (true, &e[2..]),
                                    _ => (false, &e[1..]),
                                };

                                if let Ok(s) = std::str::from_utf8(digits) {
                                    let code: Option<u32> = if hex {
                                        u32::from_str_radix(s, 16).ok()
                                    } else {
                                        s.parse().ok()
                                    };

                                    if let Some(ch) = code.and_then(char::from_u32) {
                                        let safe_char = if ch == '\u{00A0}' { ' ' } else { ch };
                                        let mut buf = [0u8; 4];
                                        dst.extend_from_slice(safe_char.encode_utf8(&mut buf).as_bytes());
                                    }
                                }

                                None
                            }
                            _ => {
                                dst.push(b'&');
                                dst.extend_from_slice(entity);
                                dst.push(b';');
                                None
                            }
                        };

                        if let Some(r) = replacement {
                            dst.extend_from_slice(r);
                        }
                    } else {
                        dst.push(b'&');
                        i = name_start;
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    match String::from_utf8(dst) {
        Ok(text) => text,
        Err(err) => String::from_utf8_lossy(&err.into_bytes()).into_owned(),
    }
}

fn fix_block_links(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(bp) = rest.find('[') {
        let is_image = bp > 0 && rest.as_bytes()[bp - 1] == b'!';
        out.push_str(&rest[..bp]);
        rest = &rest[bp..];

        let mut depth = 0i32;
        let mut end_bracket = None;
        for (idx, ch) in rest.char_indices() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end_bracket = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }

        let Some(eb) = end_bracket else {
            out.push_str(rest);
            return out;
        };

        let text = &rest[1..eb];
        let after = &rest[eb + 1..];

        if !is_image && after.starts_with('(') {
            if let Some(ep) = find_matching_paren(after) {
                if text.contains('\n') {
                    out.push_str(text);
                } else {
                    out.push('[');
                    out.push_str(text);
                    out.push(']');
                    out.push_str(&after[..=ep]);
                }
                rest = &after[ep + 1..];
                continue;
            }
        }

        out.push('[');
        rest = &rest[1..];
    }

    out.push_str(rest);
    out
}

fn find_matching_paren(input: &str) -> Option<usize> {
    let mut depth = 0i32;

    for (idx, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }

    None
}

fn post_process(raw: &str) -> String {
    let decoded = strip_and_decode(raw);
    let fixed = fix_block_links(&decoded);

    let mut result = String::with_capacity(fixed.len());
    let mut blank_streak = 0u32;

    for line in fixed.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_streak += 1;
            if blank_streak <= 1 {
                result.push('\n');
            }
        } else {
            blank_streak = 0;
            result.push_str(trimmed);
            result.push('\n');
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_search_command_accepts_query_and_flags() {
        let args = vec![
            "blitzmd".to_string(),
            "search".to_string(),
            "--limit".to_string(),
            "5".to_string(),
            "--deep".to_string(),
            "rust".to_string(),
            "async".to_string(),
        ];

        let command = parse_command(&args).expect("command should parse");
        match command {
            Command::Search(opts) => {
                assert_eq!(opts.limit, 5);
                assert!(opts.deep);
                assert_eq!(opts.query, "rust async");
            }
            _ => panic!("expected search command"),
        }
    }

    #[test]
    fn parse_search_command_accepts_llm_flag() {
        let args = vec![
            "blitzmd".to_string(),
            "search".to_string(),
            "--llm".to_string(),
            "athenox".to_string(),
        ];

        let command = parse_command(&args).expect("command should parse");
        match command {
            Command::Search(opts) => {
                assert!(opts.llm);
                assert_eq!(opts.query, "athenox");
            }
            _ => panic!("expected search command"),
        }
    }

    #[test]
    fn normalize_duckduckgo_url_decodes_redirect_target() {
        let href = "/l/?uddg=https%3A%2F%2Fexample.com%2Farticle%3Fa%3D1";
        let decoded = normalize_duckduckgo_url(href);
        assert_eq!(decoded, "https://example.com/article?a=1");
    }

    #[test]
    fn normalize_duckduckgo_url_decodes_absolute_redirect_target() {
        let href = "https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs";
        let decoded = normalize_duckduckgo_url(href);
        assert_eq!(decoded, "https://example.com/docs");
    }

    #[test]
    fn parse_bing_rss_extracts_items() {
        let xml = r#"
            <rss>
              <channel>
                <item>
                  <title>Example title</title>
                  <link>https://example.com/post</link>
                  <description><![CDATA[<b>Hello</b> &amp; welcome]]></description>
                </item>
              </channel>
            </rss>
        "#;

        let results = parse_bing_rss(xml, 10).expect("rss parsing should work");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example title");
        assert_eq!(results[0].url, "https://example.com/post");
        assert_eq!(results[0].snippet, "Hello & welcome");
    }

    #[test]
    fn merge_results_deduplicates_canonical_urls() {
        let a = SearchResult {
            title: "A".to_string(),
            url: "https://example.com/".to_string(),
            snippet: String::new(),
            source: "one".to_string(),
        };
        let b = SearchResult {
            title: "B".to_string(),
            url: "https://example.com#section".to_string(),
            snippet: String::new(),
            source: "two".to_string(),
        };
        let c = SearchResult {
            title: "C".to_string(),
            url: "https://example.org".to_string(),
            snippet: String::new(),
            source: "one".to_string(),
        };

        let merged = merge_results_round_robin(vec![vec![a, c.clone()], vec![b]], 10);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].url, "https://example.com/");
        assert_eq!(merged[1].url, c.url);
    }

    #[test]
    fn fix_block_links_handles_urls_with_parentheses() {
        let input = "[docs](https://example.com/a_(b))";
        let fixed = fix_block_links(input);
        assert_eq!(fixed, input);
    }

    #[test]
    fn has_enough_unique_results_detects_limit() {
        let g1 = vec![
            SearchResult {
                title: "A".to_string(),
                url: "https://example.com/a".to_string(),
                snippet: String::new(),
                source: "one".to_string(),
            },
            SearchResult {
                title: "B".to_string(),
                url: "https://example.com/b".to_string(),
                snippet: String::new(),
                source: "one".to_string(),
            },
        ];
        let groups = vec![g1];
        assert!(has_enough_unique_results(&groups, 2));
    }

    #[test]
    fn fastpath_requires_duckduckgo_or_all_done() {
        let metrics = vec![ProviderMetric {
            name: "bing-rss".to_string(),
            elapsed_ms: 120,
            result_count: 8,
            status: "ok".to_string(),
        }];

        assert!(!can_fastpath_with_current_providers(&metrics, false, true));
        assert!(can_fastpath_with_current_providers(&metrics, true, true));

        let metrics_with_ddg = vec![
            ProviderMetric {
                name: "bing-rss".to_string(),
                elapsed_ms: 120,
                result_count: 8,
                status: "ok".to_string(),
            },
            ProviderMetric {
                name: "duckduckgo".to_string(),
                elapsed_ms: 300,
                result_count: 8,
                status: "ok".to_string(),
            },
        ];
        assert!(can_fastpath_with_current_providers(
            &metrics_with_ddg,
            true,
            false
        ));
    }

    #[test]
    fn relevance_filter_removes_off_topic_results() {
        let results = vec![
            SearchResult {
                title: "Google Maps Help".to_string(),
                url: "https://support.google.com/maps".to_string(),
                snippet: "Get started with Maps".to_string(),
                source: "bing-rss".to_string(),
            },
            SearchResult {
                title: "Athenox Development | Innovation".to_string(),
                url: "https://athenox.dev".to_string(),
                snippet: "Agence web Athenox Development".to_string(),
                source: "duckduckgo".to_string(),
            },
        ];

        let filtered = filter_and_rank_relevant_results(&results, "Athenox Development", 8);
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].title.contains("Athenox"));
    }

    #[test]
    fn relevance_filter_returns_empty_when_nothing_matches() {
        let results = vec![SearchResult {
            title: "Visit Bologna".to_string(),
            url: "https://example.com/bologna".to_string(),
            snippet: "Travel guide".to_string(),
            source: "bing-rss".to_string(),
        }];

        let filtered = filter_and_rank_relevant_results(&results, "Athenox Development", 8);
        assert!(filtered.is_empty());
    }

    #[test]
    fn query_terms_handles_quotes() {
        let terms = query_terms("\"Athenox Development\"");
        assert_eq!(terms, vec!["athenox".to_string(), "development".to_string()]);
    }

    #[test]
    fn min_term_match_threshold_for_two_terms_requires_both() {
        assert_eq!(min_term_match_threshold(2), 2);
    }

    #[test]
    fn extract_html_title_reads_basic_title_tag() {
        let html = "<html><head><title>Athenox Development</title></head><body></body></html>";
        let title = extract_html_title(html).expect("title should be present");
        assert_eq!(title, "Athenox Development");
    }

    #[test]
    fn search_retry_triggers_when_all_providers_fail() {
        let telemetry = SearchTelemetry {
            total_elapsed_ms: 900,
            merge_elapsed_ms: 0,
            total_candidates: 0,
            unique_results: 0,
            providers: vec![
                ProviderMetric {
                    name: "duckduckgo".to_string(),
                    elapsed_ms: 900,
                    result_count: 0,
                    status: "timeout@900ms".to_string(),
                },
                ProviderMetric {
                    name: "bing-rss".to_string(),
                    elapsed_ms: 900,
                    result_count: 0,
                    status: "error: boom".to_string(),
                },
            ],
        };

        assert!(should_retry_search(&telemetry));
    }
}
