use anyhow::Result;
use pikchr::{Pikchr, PikchrFlags};
use pulldown_cmark::{html, CodeBlockKind, Event, Options, Parser, Tag};
use serde_json::json;
use std::env;
use std::fs;
use std::io;
use std::net;
use warp::Filter;

#[derive(Clone)]
struct Params {
  input: String,
  output: Option<String>,
  template: String,
}

fn usage(opts: getopts::Options) -> Result<()> {
  let brief = format!("Usage: notebook FILE [options]");
  print!("{}", opts.usage(&brief));
  Ok(())
}

fn main() -> Result<()> {
  let args: Vec<String> = env::args().collect();

  let mut opts = getopts::Options::new();
  opts.optopt("o", "output", "set output file name", "NAME");
  opts.optopt("t", "template", "template file", "TEMPLATE");
  opts.optopt("s", "serve", "serve", "HOST");
  opts.optflag("h", "help", "print this help menu");

  let matches = opts.parse(&args[1..])?;

  let input = if !matches.free.is_empty() {
    matches.free[0].clone()
  } else {
    return usage(opts);
  };

  let params = Params {
    input: input,
    output: matches.opt_str("output"),
    template: match matches.opt_str("template") {
      Some(path) => fs::read_to_string(path)?,
      None => include_str!("template.hbs").into(),
    },
  };

  match matches.opt_str("serve") {
    Some(host) => web_output(host.parse()?, params),
    None => file_output(params),
  }
}

fn file_output(params: Params) -> Result<()> {
  let input = fs::read_to_string(&params.input)?;
  let mut output: Box<dyn io::Write> = match &params.output {
    Some(path) => Box::new(fs::File::create(path)?),
    None => Box::new(io::stdout()),
  };
  render_html(&input, &params.template, &mut output)
}

#[tokio::main]
async fn web_output(addr: net::SocketAddr, params: Params) -> Result<()> {
  let not_found = || {
    let body = warp::reply::html("File not found".as_bytes().to_vec());
    let code = warp::http::StatusCode::NOT_FOUND;
    warp::reply::with_status(body, code)
  };

  let bad_request = |err: &str| {
    let body = warp::reply::html(err.as_bytes().to_vec());
    let code = warp::http::StatusCode::BAD_REQUEST;
    warp::reply::with_status(body, code)
  };

  let document_params = params.clone();
  let document = move || {
    let params = &document_params;
    let input = match fs::read_to_string(&params.input) {
      Ok(input) => input,
      Err(_) => return not_found(),
    };

    let mut buffer = vec![];
    match render_html(&input, &params.template, &mut buffer) {
      Ok(_) => {}
      Err(err) => return bad_request(&err.to_string()),
    };

    let body = warp::reply::html(buffer);
    let code = warp::http::StatusCode::OK;
    warp::reply::with_status(body, code)
  };

  let fallback_params = params.clone();
  let fallback = warp::path::tail().map(move |tail: warp::path::Tail| {
    let tail = tail.as_str();
    let params = &fallback_params;
    match (tail, &params.output.as_ref()) {
      ("", None) => document(),
      (_, Some(out)) if tail == out.as_str() => document(),
      _ => not_found(),
    }
  });

  let assets = warp::get().and(warp::fs::dir("."));
  let routes = assets.or(fallback);

  warp::serve(routes).run(addr).await;
  Ok(())
}

fn render_html<W>(input: &str, template: &str, output: &mut W) -> Result<()>
where
  W: io::Write,
{
  let mut options = Options::empty();
  options.insert(Options::ENABLE_STRIKETHROUGH);
  options.insert(Options::ENABLE_TABLES);
  options.insert(Options::ENABLE_FOOTNOTES);
  options.insert(Options::ENABLE_TASKLISTS);
  options.insert(Options::ENABLE_SMART_PUNCTUATION);

  let parser = Parser::new_ext(input, options);
  let parser = PikchrTransformer { iter: parser };
  let events = parser.into_iter().collect::<Vec<_>>();
  let heading = extract_heading(&events);

  let mut content = String::new();
  html::push_html(&mut content, events.into_iter());

  let context = json!({
      "title": heading.unwrap_or("".into()),
      "content": content,
  });

  let registry = handlebars::Handlebars::new();
  let rendered = registry.render_template(&template, &context)?;

  output.write(rendered.as_bytes())?;
  Ok(())
}

/// Extract a heading from the markdown input
fn extract_heading(events: &[Event]) -> Option<String> {
  let mut in_h1 = false;
  for event in events {
    let tag = match (in_h1, &event) {
      (false, Event::Start(tag)) => tag,
      (true, Event::Text(text)) => return Some(text.to_string()),
      _ => continue,
    };
    let kind = match tag {
      Tag::Heading(kind) => kind,
      _ => continue,
    };
    in_h1 = *kind == 1;
  }
  None
}

/// Transforms Pikchr fenced code blocks into SVG diagrams
struct PikchrTransformer<'a, T>
where
  T: Iterator<Item = Event<'a>>,
{
  iter: T,
}

impl<'a, T> Iterator for PikchrTransformer<'a, T>
where
  T: Iterator<Item = Event<'a>>,
{
  type Item = Event<'a>;

  fn next(&mut self) -> Option<Self::Item> {
    let event = match self.iter.next() {
      Some(event) => event,
      None => return None,
    };

    let tag = match event {
      Event::Start(ref tag) => tag,
      _ => return Some(event),
    };

    let kind = match tag {
      Tag::CodeBlock(kind) => kind,
      _ => return Some(event),
    };

    let lang = match kind {
      CodeBlockKind::Fenced(lang) => lang,
      _ => return Some(event),
    };

    if lang.as_ref() != "pikchr" {
      return Some(event);
    }

    let event = self
      .iter
      .next()
      .expect("Fence block to contain a text block");

    self
      .iter
      .next()
      .expect("A start event to be followed by an end event");

    let text = match event {
      Event::Text(text) => text,
      _ => unreachable!(),
    };

    // Display Pikchr syntax errors in the output document
    let event = match Pikchr::render(&text, None, PikchrFlags::default()) {
      Ok(svg) => Event::Html(svg.to_string().into()),
      Err(err) => Event::Text(err.to_string().into()),
    };

    Some(event)
  }
}
