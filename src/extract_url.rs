use indexmap::IndexSet;
use nom::branch::alt;
use nom::bytes::complete::{tag, tag_no_case, take, take_while1};
use nom::character::complete::char;
use nom::combinator::{iterator, opt, recognize, value};
use nom::multi::{many0_count, many1_count};
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
    let mut links = IndexSet::new();
    let mut stack = Vec::new();
    let mut node = dom.tree.root();
    for _ in 0..1048576_usize {
        let mut skip_children = false;
        match node.value() {
            Node::Text(text) => links.extend(extract_urls_from_text(&text)),
            Node::Element(element) => match element.name() {
                "a" => {
                    if let Some(href) = element.attr("href") {
                        skip_children = true;
                        links.extend(validate_url(href));
                    }
                }
                "code" | "del" | "mx-reply" | "pre" => skip_children = true,
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
    // A Matrix message can only be 64 KiB long.
    panic!("Bug: HTML extractor didn't stop after visiting 1 million DOM nodes.");
}

/// We follow the behavior of Element to extract URLs:
/// 1. Containing no whitespace.
/// 2. Containing balanced amounts of "()", "<>", "[]", "{}".
#[instrument]
pub fn extract_urls_from_text(text: &str) -> impl Iterator<Item = Url> {
    iterator(
        text,
        alt((
            parse_url_from_text.map(Option::Some),
            value(None, take(1_usize)),
        )),
    )
    .flatten()
    .filter_map(validate_url)
}

fn parse_url_from_text(input: &str) -> IResult<&str, &str> {
    recognize((
        tag_no_case("http"),
        opt(tag_no_case("s")),
        char(':'),
        many0_count(char('/')),
        many1_count(parse_delimited),
    ))
    .parse(input)
}

fn parse_delimited(input: &str) -> IResult<&str, ()> {
    alt((
        value((), (tag("("), many0_count(parse_delimited), opt(tag(")")))),
        value((), (tag("<"), many0_count(parse_delimited), opt(tag(">")))),
        value((), (tag("["), many0_count(parse_delimited), opt(tag("]")))),
        value((), (tag("{"), many0_count(parse_delimited), opt(tag("}")))),
        value(
            (),
            take_while1(|c| {
                !matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | '{' | '}')
                    && !char::is_whitespace(c)
            }),
        ),
    ))
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
