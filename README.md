# Blitz.md

**Blazing-fast URL → Markdown converter written in Rust.**

Fetch any webpage and get clean, readable Markdown in milliseconds. Streams the HTML, parses and converts on the fly — no intermediate DOM, no bloat.

```bash
blitzmd https://example.com > output.md
```

---

## Features

- **Streaming pipeline** — converts HTML chunks as they arrive, no wait for full download
- **Zero-copy post-processing** — bulk byte-slice operations, single pass for tag stripping + entity decoding
- **Absolute URL resolution** — all links and images resolved to full URLs
- **Block link flattening** — `<a>` used as card containers become readable text, not broken multi-line links
- **Full HTML entity support** — named (`&amp;`, `&nbsp;`, `&mdash;`…) and numeric (`&#160;`, `&#x2014;`…)
- **Smart noise removal** — strips `<script>`, `<style>`, `<nav>`, `<footer>`, `<svg>`, `<iframe>` and form elements
- **Brotli + gzip compression** — fastest possible transfer
- **`--verbose` timing mode** — inspect each pipeline stage

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

```
blitzmd [--verbose] <URL>
```

```bash
# Basic conversion
blitzmd https://news.ycombinator.com

# Save to file
blitzmd https://en.wikipedia.org/wiki/Rust_(programming_language) > rust.md

# Show timing breakdown
blitzmd --verbose https://example.com
```

### Verbose output

```
[timing] connect+headers : 352ms
[timing] body+parse      : 2.60ms
[timing] raw size        : 7 KB
[timing] post_process    : 24µs
[timing] total           : 355ms
```

---

## How it works

```
URL
 │
 ▼
reqwest (gzip/brotli, tcp_nodelay)
 │  streaming chunks
 ▼
lol_html (streaming HTML rewriter)
 │  • removes noise tags entirely (script, style, nav, footer…)
 │  • injects markdown markers around content tags (h1→# , strong→**, a→[]()…)
 │  • resolves relative URLs on the fly
 ▼
strip_and_decode()          ← single pass, bulk byte-slice copy
 │  • strips residual HTML tags
 │  • decodes HTML entities
 ▼
fix_block_links()           ← flattens multi-line [text](url) into plain text
 ▼
normalize_lines()           ← trim + collapse blank lines
 ▼
stdout
```

The entire post-processing pipeline runs in **~25–800µs** depending on page size.

---

## Benchmarks

All benchmarks run on the same machine, conversion only (network excluded), against real pages.

### Conversion speed — same HTML input

| Page | Size | Blitz.md | html2text | trafilatura |
|------|------|----------|-----------|-------------|
| Wikipedia — Rust lang | 207 KB | **813 µs** | 169 ms | 577 ms |
| Le Monde | 82 KB | **315 µs** | 65 ms | 71 ms |
| GitHub — linux/linux | 30 KB | **116 µs** | 45 ms | 128 ms |
| Hacker News | 14 KB | **65 µs** | 14 ms | 46 ms |
| Python docs | 8 KB | **55 µs** | 9 ms | 18 ms |

Blitz.md is **165× to 1100× faster** than Python-based alternatives on conversion.

### End-to-end (fetch + convert) vs Jina AI Reader

| Page | Blitz.md | Jina AI |
|------|----------|---------|
| oxalisheberg.fr | **522 ms** | 338 ms |
| Python docs | 439 ms | **439 ms** |
| Hacker News | **760 ms** | 760 ms |

Jina edges out on latency for some URLs thanks to distributed infrastructure and caching. Blitz.md runs entirely on your machine — no API key, no rate limits, no data leaving your network.

---

## Comparison

| | Blitz.md | html2text | trafilatura | Jina AI Reader |
|---|---|---|---|---|
| Language | Rust | Python | Python | Cloud API |
| Conversion speed | **~25–800 µs** | 9–170 ms | 18–580 ms | N/A (remote) |
| Streaming | Yes | No | No | No |
| Local / offline | Yes | Yes | Yes | No |
| Private URLs | Yes | Yes | Yes | No |
| Rate limits | None | None | None | Yes |
| API key required | No | No | No | Yes (paid tiers) |
| Binary size | ~5 MB | — | — | — |

---

## License

MIT
