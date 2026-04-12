# Blitz.md

**Blazing-fast URL -> Markdown converter written in Rust.**

Fetch any webpage and get clean, readable Markdown in milliseconds. Streams the HTML, parses and converts on the fly - no intermediate DOM, no bloat.

```bash
blitzmd https://example.com > output.md
```

---

## Features

- **Streaming pipeline** - converts HTML chunks as they arrive, no wait for full download
- **Zero-copy post-processing** - bulk byte-slice operations, single pass for tag stripping + entity decoding
- **Absolute URL resolution** - all links and images resolved to full URLs
- **Block link flattening** - `<a>` used as card containers become readable text, not broken multi-line links
- **Full HTML entity support** - named (`&amp;`, `&nbsp;`, `&mdash;`...) and numeric (`&#160;`, `&#x2014;`...)
- **Smart noise removal** - strips `<script>`, `<style>`, `<nav>`, `<footer>`, `<svg>`, `<iframe>` and form elements
- **Brotli + gzip compression** - fastest possible transfer
- **`--verbose` timing mode** - inspect each pipeline stage

---

## Installation

### Pre-built binaries

Download the latest release for your platform from the [Releases](../../releases) page.

### Build from source

```bash
git clone https://github.com/Athenox14/Blitz.md
cd Blitz.md
cargo build --release
# binary: ./target/release/blitzmd
```

Requires Rust 1.75+.

---

## Usage

```text
blitzmd [--verbose] <URL>
blitzmd search [--limit N] [--timeout-ms MS] [--json] [--llm] [--deep] [--deep-limit N] [--verbose] <query>
```

### Ultra-fast web search

```bash
# Fast metasearch (parallel providers + dedup)
blitzmd search "rust async runtime"

# LLM-ready output + telemetry
blitzmd search --llm --verbose "Athenox Development"

# JSON output for scripts
blitzmd search --json --limit 10 "vector database benchmark"

# Enrich top results by converting pages to markdown excerpts
blitzmd search --llm --verbose --deep --deep-limit 3 "tokio vs async-std"
```

### Search behavior and concurrency

- DuckDuckGo and Bing RSS are queried in parallel.
- Results are deduplicated and merged.
- Fast-path can stop waiting early when confidence is high.
- Relevance filtering removes off-topic noise.
- Automatic rescue strategies run when providers are unstable:
- wider-timeout retry,
- targeted query rescue,
- domain discovery fallback.

### Search speed tips

- Lowest latency: avoid `--deep`, lower `--limit`, reduce `--timeout-ms`.
- Highest quality under network jitter: keep default timeout or increase (`1500-2500ms`).
- `--deep` adds parallel page fetch/conversion cost but improves validation quality.

### Verbose search telemetry

`--verbose` for search prints:

- provider status (`ok`, `empty`, `timeout`, `error`, `skipped-fastpath`),
- provider timings,
- merge timing,
- search throughput,
- deep throughput and per-page averages when `--deep` is enabled.

### Basic conversion

```bash
# Basic conversion
blitzmd https://news.ycombinator.com

# Save to file
blitzmd https://en.wikipedia.org/wiki/Rust_(programming_language) > rust.md

# Show timing breakdown
blitzmd --verbose https://example.com
```

### Verbose output

```text
[timing] connect+headers : 352ms
[timing] body+parse      : 2.60ms
[timing] raw size        : 7 KB
[timing] post_process    : 24us
[timing] total           : 355ms
```

### Real-world search performance (sample)

Measured from user runs on Windows (network-dependent):

| Command | Typical observed total |
|---|---:|
| `search --llm --verbose` | ~180 ms to ~900 ms |
| `search --llm --verbose` with retries | ~1.7 s to ~2.4 s |
| `search --llm --verbose --deep --deep-limit 3` | ~640 ms to ~1.0 s |

---

## How it works

```text
URL
 |
 v
reqwest (gzip/brotli, tcp_nodelay)
 |  streaming chunks
 v
lol_html (streaming HTML rewriter)
 |  - removes noise tags entirely (script, style, nav, footer...)
 |  - injects markdown markers around content tags (h1-># , strong->**, a->[]()...)
 |  - resolves relative URLs on the fly
 v
strip_and_decode()          <- single pass, bulk byte-slice copy
 |  - strips residual HTML tags
 |  - decodes HTML entities
 v
fix_block_links()           <- flattens multi-line [text](url) into plain text
 v
normalize_lines()           <- trim + collapse blank lines
 v
stdout
```

The entire post-processing pipeline runs in **~25-800us** depending on page size.

---

## Benchmarks

All benchmarks run on the same machine, conversion only (network excluded), against real pages.

### Conversion speed - same HTML input

| Page | Size | Blitz.md | html2text | trafilatura |
|------|------|----------|-----------|-------------|
| Wikipedia - Rust lang | 207 KB | **813 us** | 169 ms | 577 ms |
| Le Monde | 82 KB | **315 us** | 65 ms | 71 ms |
| GitHub - linux/linux | 30 KB | **116 us** | 45 ms | 128 ms |
| Hacker News | 14 KB | **65 us** | 14 ms | 46 ms |
| Python docs | 8 KB | **55 us** | 9 ms | 18 ms |

Blitz.md is **165x to 1100x faster** than Python-based alternatives on conversion.

### End-to-end (fetch + convert) vs Jina AI Reader

| Page | Blitz.md | Jina AI |
|------|----------|---------|
| oxalisheberg.fr | **338 ms** | 549 ms |
| Python docs | **339 ms** | 439 ms |
| Hacker News | **660 ms** | 776 ms |

Jina edges out on latency for some URLs thanks to distributed infrastructure and caching. Blitz.md runs entirely on your machine - no API key, no rate limits, no data leaving your network.

---

## Comparison

| | Blitz.md | html2text | trafilatura | Jina AI Reader |
|---|---|---|---|---|
| Language | Rust | Python | Python | Cloud API |
| Conversion speed | **~25-800 us** | 9-170 ms | 18-580 ms | N/A (remote) |
| Streaming | Yes | No | No | No |
| Local / offline | Yes | Yes | Yes | No |
| Private URLs | Yes | Yes | Yes | No |
| Rate limits | None | None | None | Yes |
| API key required | No | No | No | Yes (paid tiers) |
| Binary size | ~5 MB | - | - | - |

---

## Development

```bash
cargo test
```

If your workspace blocks writes in `target/` (common in synced folders):

```powershell
$env:CARGO_TARGET_DIR="$env:TEMP\\blitzmd-target"
cargo test
```

---

## License

MIT
