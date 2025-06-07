use indexmap::IndexSet;
use nom::branch::alt;
use nom::bytes::complete::{tag, tag_no_case, take, take_while1};
use nom::character::complete::char;
use nom::combinator::{iterator, opt, recognize, value};
use nom::multi::{many_till, many0_count, many1_count};
use nom::sequence::preceded;
use nom::{IResult, Parser};
use scraper::{Html, Node};
use tracing::instrument;
use url::{Host, Url};

use crate::common::SAFE_URL_LENGTH;

/// Extracts URLs from *both* <a href="URL"> and the text contents.
///
/// Text contents are processed by [`extract_urls_from_text`].
#[instrument]
pub fn extract_urls_from_html(html: &str) -> IndexSet<Url> {
    let dom = Html::parse_fragment(html);
    let mut stack = Vec::new();
    let mut links = IndexSet::new();

    let mut node = dom.tree.root();
    let mut counter = 0_usize;
    loop {
        counter += 1;
        if counter >= 1048576 {
            // A Matrix message can only be 64 KiB long.
            panic!("Bug: HTML extractor didn't stop after visiting 1 million DOM nodes.");
        }

        let mut skip_children = false;
        match node.value() {
            Node::Text(text) => links.extend(extract_urls_from_text(&text)),
            Node::Element(element) => match element.name().to_ascii_lowercase().as_str() {
                "a" => {
                    for (k, v) in element.attrs() {
                        if k.eq_ignore_ascii_case("href") {
                            skip_children = true;
                            links.extend(validate_url(v));
                            break;
                        }
                    }
                }
                "blockquote" | "code" | "del" | "mx-reply" | "pre" => skip_children = true,
                _ => (),
            },
            _ => (),
        }
        if !skip_children {
            if let Some(child) = node.first_child() {
                stack.push(node);
                node = child;
                continue;
            }
        }
        loop {
            if let Some(sibling) = node.next_sibling() {
                node = sibling;
                break;
            } else if let Some(parent) = stack.pop() {
                node = parent;
            } else {
                return links;
            }
        }
    }
}

/// We use the CommmonMark definition to extract URLs:
/// 1. Either a `<URL>`, where `URL` contains no "<", ">", or whitespaces.
/// 2. Or a bare `URL`, where `URL` can contain a balanced amount of "()", but terminated by any whitespace.
///
/// In both situations, we additionally forbid "<" or ">" inside `URL`, which is not in the CommonMark specification.
#[instrument]
pub fn extract_urls_from_text(text: &str) -> impl Iterator<Item = Url> {
    iterator(
        text,
        many_till(value((), take(1_usize)), parse_url_from_text).map(|(_skipped, parsed)| parsed),
    )
    .filter_map(validate_url)
}

#[instrument]
fn parse_url_from_text(input: &str) -> IResult<&str, &str> {
    alt((parse_url_in_angle_brackets, parse_url_bare)).parse(input)
}

#[instrument]
fn parse_url_in_angle_brackets(input: &str) -> IResult<&str, &str> {
    preceded(
        char('<'),
        recognize((
            tag_no_case("http"),
            opt(tag_no_case("s")),
            char(':'),
            many0_count(char('/')),
            take_while1(|c| c != '<' && c != '>' && !char::is_whitespace(c)),
        )),
    )
    .parse(input)
}

#[instrument]
fn parse_url_bare(input: &str) -> IResult<&str, &str> {
    recognize((
        tag_no_case("http"),
        opt(tag_no_case("s")),
        char(':'),
        many0_count(char('/')),
        many1_count(alt((parse_parenthesis, parse_non_parenthesis))),
    ))
    .parse(input)
}

#[instrument]
fn parse_parenthesis(input: &str) -> IResult<&str, &str> {
    recognize((tag("("), parse_non_parenthesis, opt(tag(")")))).parse(input)
}

#[instrument]
fn parse_non_parenthesis(input: &str) -> IResult<&str, &str> {
    take_while1(|c| c != '(' && c != ')' && c != '<' && c != '>' && !char::is_whitespace(c))
        .parse(input)
}

#[instrument]
pub fn validate_url(url: &str) -> Option<Url> {
    let mut url = Url::parse(url).ok()?;
    // https://stackoverflow.com/a/417184/2557927
    if url.as_str().len() > SAFE_URL_LENGTH {
        return None;
    }
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host()?;
    if let Host::Domain(domain) = host {
        // Matrix mentions generate <a href="https://matrix.to/#[...]"> links. Ignore them.
        if domain.eq_ignore_ascii_case("matrix.to") {
            return None;
        }
    }
    // Make sure the `#fragment` part is kept private.
    url.set_fragment(None);
    Some(url)
}
