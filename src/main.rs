/*
 * Copyright Carl Lei, 2024.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::io::{BufRead, ErrorKind::*};
use std::path::Path as StdPath;
use std::time::SystemTime;
use std::{mem::ManuallyDrop, os::fd::FromRawFd};

use async_trait::async_trait;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{Html, IntoResponseParts, Redirect, Response, ResponseParts};
use axum::{extract::Path, http::StatusCode, response::IntoResponse, Router};
use httpdate::HttpDate;
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Deserialize;

fn main() {
    let sock = ManuallyDrop::new(unsafe { std::net::TcpStream::from_raw_fd(0) });
    let Ok(_sa) = sock.local_addr() else {
        return;
    };
    sock.set_nonblocking(true).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let tokiosock =
                tokio::net::TcpStream::from_std(ManuallyDrop::into_inner(sock)).unwrap();
            let io = TokioIo::new(tokiosock);
            let hs = hyper_util::service::TowerToHyperService::new(routes().into_service());
            hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(io, hs)
                .await
                .ok();
        });
}

fn routes() -> Router {
    use axum::routing::*;
    Router::new()
        .route("/:section/:name", get(render))
        .route("/:name", get(find))
}

#[derive(Deserialize)]
struct ManPath {
    section: String,
    name: String,
}

async fn find(Path(name): Path<String>) -> Result<Response, StatusCode> {
    name.rsplit_once('.')
        .filter(|(_, section)| {
            *section == "n" || section.starts_with(|c: char| c.is_ascii_digit())
        })
        .or_else(|| {
            Some((
                &name[..],
                ["1", "8", "6", "2", "3", "5", "7", "4", "9", "3p"]
                    .into_iter()
                    .find(|section| {
                        std::fs::exists(format!("/usr/share/man/man{section}/{name}.{section}.gz"))
                            .unwrap_or_default()
                    })?,
            ))
        })
        .map(|(name, section)| {
            Redirect::temporary(&format!("/{section}/{name}.{section}.html")).into_response()
        })
        .ok_or(StatusCode::NOT_FOUND)
}

async fn render(
    Path(ManPath { section, name }): Path<ManPath>,
    IfChangedSince(when): IfChangedSince,
) -> Result<Response, StatusCode> {
    let name = name.strip_suffix(".html").ok_or(StatusCode::NOT_FOUND)?;
    let fp = format!("/usr/share/man/man{section}/{name}.gz");
    let date = bg({
        let fp = fp.clone();
        move || std::fs::metadata(&fp)
    })
    .await
    .and_then(|m| m.modified())
    .map_err(conv_ioe)?;
    // on my system, mtime of manpages seems to have second resolution.
    if when.is_some_and(|when| when >= date) {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let so = bg({
        let fp = fp.clone();
        move || check_so(fp.as_ref())
    })
    .await
    .map_err(conv_ioe)?;
    if let Some(so) = so {
        let part = so.strip_prefix("man").ok_or(StatusCode::NOT_FOUND)?;
        let dst = format!("/{part}.html");
        Ok((SetDate(date), Redirect::temporary(&dst)).into_response())
    } else {
        Ok((
            SetDate(date),
            Html(bg(move || format_reply(&fp)).await.map_err(conv_ioe)?),
        )
            .into_response())
    }
}

fn conv_ioe(e: std::io::Error) -> StatusCode {
    match e.kind() {
        NotFound => StatusCode::NOT_FOUND,
        PermissionDenied => StatusCode::FORBIDDEN,
        _ => {
            eprintln!("IO Error: {e:?}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

fn check_so(p: &StdPath) -> Result<Option<String>, std::io::Error> {
    let f = std::fs::File::open(p)?;
    let dec = flate2::read::GzDecoder::new(f);
    let mut decr = std::io::BufReader::new(dec);
    let mut line = Default::default();
    decr.read_line(&mut line)?;
    if line.ends_with('\n') {
        line.pop();
    }
    if line.starts_with(".so ") {
        line.replace_range(..4, "");
        Ok(Some(line))
    } else {
        Ok(None)
    }
}

fn format_reply(p: &str) -> Result<String, std::io::Error> {
    let body = String::from_utf8(
        std::process::Command::new("mandoc")
            .args(["-T", "html", "-O", "fragment,man=/%S/%N.%S.html", p])
            .output()?
            .stdout,
    )
    .or(Err(InvalidData))?;
    Ok(PAGE_PRE.to_owned() + &body + PAGE_POST)
}

async fn bg<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(f).await.unwrap()
}

struct IfChangedSince(Option<SystemTime>);

#[async_trait]
impl<S: Send + Sync> FromRequestParts<S> for IfChangedSince {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        use axum::http::header;
        Ok(Self(
            parts
                .headers
                .get(header::IF_MODIFIED_SINCE)
                .map(|s| httpdate::parse_http_date(s.to_str().unwrap_or_default()))
                .transpose()
                .map_err(|_| (StatusCode::BAD_REQUEST, "cannot parse If-Modified-Since"))?,
        ))
    }
}

struct SetDate(SystemTime);

impl IntoResponseParts for SetDate {
    type Error = StatusCode;

    fn into_response_parts(self, mut res: ResponseParts) -> Result<ResponseParts, Self::Error> {
        use axum::http::header;
        res.headers_mut().insert(
            header::DATE,
            HttpDate::from(self.0)
                .to_string()
                .parse()
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
        );
        Ok(res)
    }
}

static PAGE_PRE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1.0"/>
<link rel="stylesheet" href="/style.css" type="text/css" media="all">
</head>
<body>
"#;

static PAGE_POST: &str = r#"
</body>
</html>
"#;
