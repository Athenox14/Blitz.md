use anyhow::{Context, Result};
use futures_util::StreamExt;
use lol_html::{element, html_content::ContentType, HtmlRewriter, Settings};
use std::env;
use std::time::Instant;

struct Sink<'a>(&'a mut String);

impl<'a> lol_html::OutputSink for Sink<'a> {
    fn handle_chunk(&mut self, chunk: &[u8]) {
        self.0.push_str(&String::from_utf8_lossy(chunk));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");
    let url = args.iter().skip(1).find(|a| !a.starts_with('-'));

    let Some(url) = url else {
        eprintln!("blitzmd — blazing-fast URL to Markdown converter");
        eprintln!();
        eprintln!("Usage: {} [--verbose] <URL>", args[0]);
        std::process::exit(1);
    };

    let t0 = Instant::now();

    let client = reqwest::Client::builder()
        .gzip(true)
        .brotli(true)
        .tcp_nodelay(true)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .build()?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to fetch: {}", url))?;

    if !response.status().is_success() {
        anyhow::bail!("HTTP error: {}", response.status());
    }

    if verbose {
        eprintln!("[timing] connect+headers : {:.2?}", t0.elapsed());
    }

    let mut body_stream = response.bytes_stream();
    let mut raw = String::with_capacity(64 * 1024);

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("script",   |el| { el.remove(); Ok(()) }),
                element!("style",    |el| { el.remove(); Ok(()) }),
                element!("head",     |el| { el.remove(); Ok(()) }),
                element!("nav",      |el| { el.remove(); Ok(()) }),
                element!("footer",   |el| { el.remove(); Ok(()) }),
                element!("iframe",   |el| { el.remove(); Ok(()) }),
                element!("noscript", |el| { el.remove(); Ok(()) }),
                element!("svg",      |el| { el.remove(); Ok(()) }),
                element!("input",    |el| { el.remove(); Ok(()) }),
                element!("select",   |el| { el.remove(); Ok(()) }),
                element!("textarea", |el| { el.remove(); Ok(()) }),
                element!("button",   |el| { el.remove(); Ok(()) }),

                element!("h1", |el| { el.before("\n\n# ",      ContentType::Text); el.after("\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("h2", |el| { el.before("\n\n## ",     ContentType::Text); el.after("\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("h3", |el| { el.before("\n\n### ",    ContentType::Text); el.after("\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("h4", |el| { el.before("\n\n#### ",   ContentType::Text); el.after("\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("h5", |el| { el.before("\n\n##### ",  ContentType::Text); el.after("\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("h6", |el| { el.before("\n\n###### ", ContentType::Text); el.after("\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),

                element!("p",          |el| { el.before("\n\n",      ContentType::Text); el.after("\n\n",    ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("blockquote", |el| { el.before("\n\n> ",    ContentType::Text); el.after("\n\n",    ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("pre",        |el| { el.before("\n\n```\n", ContentType::Text); el.after("\n```\n\n",ContentType::Text); el.remove_and_keep_content(); Ok(()) }),

                element!("ul", |el| { el.before("\n",   ContentType::Text); el.after("\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("ol", |el| { el.before("\n",   ContentType::Text); el.after("\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("li", |el| { el.before("\n* ", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),

                element!("strong", |el| { el.before("**", ContentType::Text); el.after("**", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("b",      |el| { el.before("**", ContentType::Text); el.after("**", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("em",     |el| { el.before("_",  ContentType::Text); el.after("_",  ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("i",      |el| { el.before("_",  ContentType::Text); el.after("_",  ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("code",   |el| { el.before("`",  ContentType::Text); el.after("`",  ContentType::Text); el.remove_and_keep_content(); Ok(()) }),

                element!("a", |el| {
                    let href = el.get_attribute("href").unwrap_or_default();
                    if href.is_empty() || href.starts_with('#') {
                        el.remove_and_keep_content();
                    } else {
                        let abs = resolve_url(url, &href);
                        el.before("[", ContentType::Text);
                        el.after(&format!("]({})", abs), ContentType::Text);
                        el.remove_and_keep_content();
                    }
                    Ok(())
                }),

                element!("img", |el| {
                    let src = el.get_attribute("src").unwrap_or_default();
                    if !src.is_empty() {
                        let alt = el.get_attribute("alt").unwrap_or_default();
                        el.before(&format!("![{}]({})", alt, resolve_url(url, &src)), ContentType::Text);
                    }
                    el.remove();
                    Ok(())
                }),

                element!("br", |el| { el.before("\n",          ContentType::Text); el.remove(); Ok(()) }),
                element!("hr", |el| { el.before("\n\n---\n\n", ContentType::Text); el.remove(); Ok(()) }),

                element!("table",  |el| { el.before("\n\n", ContentType::Text); el.after("\n\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("tr",     |el| { el.before("\n",    ContentType::Text); el.after(" |", ContentType::Text);  el.remove_and_keep_content(); Ok(()) }),
                element!("th",     |el| { el.before("| **",  ContentType::Text); el.after("** ",ContentType::Text);  el.remove_and_keep_content(); Ok(()) }),
                element!("td",     |el| { el.before("| ",    ContentType::Text); el.after(" ",  ContentType::Text);  el.remove_and_keep_content(); Ok(()) }),
                element!("thead",  |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("tbody",  |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("tfoot",  |el| { el.remove_and_keep_content(); Ok(()) }),

                element!("dl", |el| { el.before("\n\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("dt", |el| { el.before("\n**", ContentType::Text); el.after("**", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("dd", |el| { el.before("\n: ", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),

                element!("figure",     |el| { el.before("\n",  ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("figcaption", |el| { el.before("\n_", ContentType::Text); el.after("_\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),

                element!("html",     |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("body",     |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("main",     |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("header",   |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("aside",    |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("span",     |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("label",    |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("form",     |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("fieldset", |el| { el.remove_and_keep_content(); Ok(()) }),
                element!("div",      |el| { el.before("\n",   ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("section",  |el| { el.before("\n\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
                element!("article",  |el| { el.before("\n\n", ContentType::Text); el.remove_and_keep_content(); Ok(()) }),
            ],
            ..Default::default()
        },
        Sink(&mut raw),
    );

    let t_body = Instant::now();
    while let Some(chunk) = body_stream.next().await {
        rewriter.write(&chunk.context("Stream read error")?).context("HTML parse error")?;
    }
    rewriter.end().context("HTML finalization error")?;

    if verbose {
        eprintln!("[timing] body+parse      : {:.2?}", t_body.elapsed());
        eprintln!("[timing] raw size        : {} KB", raw.len() / 1024);
    }

    let t_post = Instant::now();
    let result = post_process(&raw);

    if verbose {
        eprintln!("[timing] post_process    : {:.2?}", t_post.elapsed());
        eprintln!("[timing] total           : {:.2?}", t0.elapsed());
    }

    println!("{}", result);
    Ok(())
}

fn resolve_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") || href.starts_with("//") {
        return href.to_string();
    }
    let origin = base
        .find("://")
        .map(|i| {
            let rest = &base[i + 3..];
            &base[..i + 3 + rest.find('/').unwrap_or(rest.len())]
        })
        .unwrap_or(base);
    if href.starts_with('/') {
        format!("{}{}", origin, href)
    } else {
        let base_dir = base.rfind('/').map(|i| &base[..=i]).unwrap_or(base);
        format!("{}{}", base_dir, href)
    }
}

fn strip_and_decode(input: &str) -> String {
    let src = input.as_bytes();
    let mut dst: Vec<u8> = Vec::with_capacity(input.len());
    let mut i = 0;
    let mut in_tag = false;

    while i < src.len() {
        if in_tag {
            match src[i..].iter().position(|&b| b == b'>') {
                Some(pos) => { i += pos + 1; in_tag = false; }
                None => break,
            }
        } else {
            let start = i;
            while i < src.len() && src[i] != b'<' && src[i] != b'&' {
                i += 1;
            }
            dst.extend_from_slice(&src[start..i]);

            if i >= src.len() { break; }

            match src[i] {
                b'<' => { in_tag = true; i += 1; }
                b'&' => {
                    i += 1;
                    let name_start = i;
                    let semi = src[i..].iter().take(12).position(|&b| b == b';');
                    if let Some(len) = semi {
                        let entity = &src[i..i + len];
                        i += len + 1;
                        let replacement: Option<&[u8]> = match entity {
                            b"amp"    => Some(b"&"),
                            b"lt"     => Some(b"<"),
                            b"gt"     => Some(b">"),
                            b"quot"   => Some(b"\""),
                            b"apos"   => Some(b"'"),
                            b"nbsp"   => Some(b" "),
                            b"mdash"  => Some("—".as_bytes()),
                            b"ndash"  => Some("–".as_bytes()),
                            b"hellip" => Some("…".as_bytes()),
                            b"laquo"  => Some("«".as_bytes()),
                            b"raquo"  => Some("»".as_bytes()),
                            b"copy"   => Some("©".as_bytes()),
                            b"reg"    => Some("®".as_bytes()),
                            b"trade"  => Some("™".as_bytes()),
                            b"euro"   => Some("€".as_bytes()),
                            b"eacute" => Some("é".as_bytes()),
                            b"egrave" => Some("è".as_bytes()),
                            b"ecirc"  => Some("ê".as_bytes()),
                            b"agrave" => Some("à".as_bytes()),
                            b"ugrave" => Some("ù".as_bytes()),
                            b"ccedil" => Some("ç".as_bytes()),
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
                                        let ch = if ch == '\u{00A0}' { ' ' } else { ch };
                                        let mut buf = [0u8; 4];
                                        dst.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
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

    // SAFETY: input is valid UTF-8; all entity replacements are valid UTF-8 literals.
    unsafe { String::from_utf8_unchecked(dst) }
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
        for (i, ch) in rest.char_indices() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 { end_bracket = Some(i); break; }
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
            if let Some(ep) = after.find(')') {
                if text.contains('\n') {
                    out.push_str(text);
                } else {
                    out.push('[');
                    out.push_str(text);
                    out.push(']');
                    out.push_str(&after[..ep + 1]);
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

fn post_process(raw: &str) -> String {
    let decoded = strip_and_decode(raw);
    let fixed = fix_block_links(&decoded);

    let mut result = String::with_capacity(fixed.len());
    let mut blank_streak = 0u32;

    for line in fixed.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank_streak += 1;
            if blank_streak <= 1 { result.push('\n'); }
        } else {
            blank_streak = 0;
            result.push_str(t);
            result.push('\n');
        }
    }

    result.trim().to_string()
}
