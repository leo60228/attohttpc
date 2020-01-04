#![allow(dead_code)]
#![allow(clippy::write_with_newline)]
use std::borrow::Borrow;
use std::borrow::Cow;
use std::convert::From;
use std::convert::TryInto;
use std::io::{prelude::*, BufWriter};
use std::str;

#[cfg(feature = "compress")]
use http::header::ACCEPT_ENCODING;
use http::{
    header::{HeaderValue, IntoHeaderName, ACCEPT, CONNECTION, CONTENT_LENGTH, HOST, USER_AGENT},
    HeaderMap, Method, Version,
};
use lazy_static::lazy_static;
use url::Url;

#[cfg(feature = "charsets")]
use crate::charsets::Charset;
use crate::error::{Error, ErrorKind, InvalidResponseKind, Result};
use crate::parsing::{parse_response, Response};
use crate::streams::BaseStream;

const VERSION: &'static str = env!("CARGO_PKG_VERSION");

lazy_static! {
    static ref DEFAULT_USER_AGENT: String = format!("attohttpc/{}", VERSION);
}

fn header_insert<H, V>(headers: &mut HeaderMap, header: H, value: V) -> Result
where
    H: IntoHeaderName,
    V: TryInto<HeaderValue>,
    Error: From<V::Error>,
{
    let value = value.try_into()?;
    headers.insert(header, value);
    Ok(())
}

fn header_insert_if_missing<H, V>(headers: &mut HeaderMap, header: H, value: V) -> Result
where
    H: IntoHeaderName,
    V: TryInto<HeaderValue>,
    Error: From<V::Error>,
{
    let value = value.try_into()?;
    headers.entry(header).or_insert(value);
    Ok(())
}

fn header_append<H, V>(headers: &mut HeaderMap, header: H, value: V) -> Result
where
    H: IntoHeaderName,
    V: TryInto<HeaderValue>,
    Error: From<V::Error>,
{
    let value = value.try_into()?;
    headers.append(header, value);
    Ok(())
}

/// `Request` is the main way of performing requests.
///
/// You can create a `RequestBuilder` the hard way using the `new` or `try_new` method,
/// or use one of the simpler constructors available in the crate root, such as `get`
/// `post`, etc.
#[derive(Debug)]
pub struct RequestBuilder<B = [u8; 0]> {
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: B,
    max_redirections: u32,
    follow_redirects: bool,
    #[cfg(feature = "charsets")]
    pub(crate) default_charset: Option<Charset>,
    #[cfg(feature = "compress")]
    allow_compression: bool,
}

impl RequestBuilder {
    /// Create a new `Request` with the base URL and the given method.
    ///
    /// # Panics
    /// Panics if the base url is invalid or if the method is CONNECT.
    pub fn new<U>(method: Method, base_url: U) -> Self
    where
        U: AsRef<str>,
    {
        Self::try_new(method, base_url).expect("invalid url or method")
    }

    /// Try to create a new `RequestBuilder`.
    ///
    /// If the base URL is invalid, an error is returned.
    /// If the method is CONNECT, an error is also returned. CONNECT is not yet supported.
    pub fn try_new<U>(method: Method, base_url: U) -> Result<Self>
    where
        U: AsRef<str>,
    {
        let url = Url::parse(base_url.as_ref()).map_err(|_| ErrorKind::InvalidBaseUrl)?;

        if method == Method::CONNECT {
            return Err(ErrorKind::ConnectNotSupported.into());
        }

        Ok(Self {
            url,
            method,
            headers: HeaderMap::new(),
            body: [],
            max_redirections: 5,
            follow_redirects: true,
            #[cfg(feature = "charsets")]
            default_charset: None,
            #[cfg(feature = "compress")]
            allow_compression: true,
        })
    }
}

impl<B> RequestBuilder<B> {
    /// Associate a query string parameter to the given value.
    ///
    /// The same key can be used multiple times.
    pub fn param<K, V>(mut self, key: K, value: V) -> Self
    where
        K: AsRef<str>,
        V: ToString,
    {
        self.url.query_pairs_mut().append_pair(key.as_ref(), &value.to_string());
        self
    }

    /// Associated a list of pairs to query parameters.
    ///
    /// The same key can be used multiple times.
    ///
    /// # Example
    /// ```
    /// attohttpc::get("http://foo.bar").params(&[("p1", "v1"), ("p2", "v2")]);
    /// ```
    pub fn params<P, K, V>(mut self, pairs: P) -> Self
    where
        P: IntoIterator,
        P::Item: Borrow<(K, V)>,
        K: AsRef<str>,
        V: ToString,
    {
        for pair in pairs.into_iter() {
            let (key, value) = pair.borrow();
            self.url.query_pairs_mut().append_pair(key.as_ref(), &value.to_string());
        }
        self
    }

    /// Modify a header for this `Request`.
    ///
    /// If the header is already present, the value will be replaced. If you wish to append a new header,
    /// use `header_append`.
    ///
    /// # Panics
    /// This method will panic if the value is invalid.
    pub fn header<H, V>(self, header: H, value: V) -> Self
    where
        H: IntoHeaderName,
        V: TryInto<HeaderValue>,
        Error: From<V::Error>,
    {
        self.try_header(header, value).expect("invalid header value")
    }

    /// Modify a header for this `Request`.
    ///
    /// If the header is already present, the value will be replaced. If you wish to append a new header,
    /// use `header_append`.
    ///
    /// # Panics
    /// This method will panic if the value is invalid.
    pub fn header_append<H, V>(self, header: H, value: V) -> Self
    where
        H: IntoHeaderName,
        V: TryInto<HeaderValue>,
        Error: From<V::Error>,
    {
        self.try_header_append(header, value).expect("invalid header value")
    }

    /// Modify a header for this `Request`.
    ///
    /// If the header is already present, the value will be replaced. If you wish to append a new header,
    /// use `header_append`.
    pub fn try_header<H, V>(mut self, header: H, value: V) -> Result<Self>
    where
        H: IntoHeaderName,
        V: TryInto<HeaderValue>,
        Error: From<V::Error>,
    {
        header_insert(&mut self.headers, header, value)?;
        Ok(self)
    }

    /// Append a new header to this `Request`.
    ///
    /// The new header is always appended to the `Request`, even if the header already exists.
    pub fn try_header_append<H, V>(mut self, header: H, value: V) -> Result<Self>
    where
        H: IntoHeaderName,
        V: TryInto<HeaderValue>,
        Error: From<V::Error>,
    {
        header_append(&mut self.headers, header, value)?;
        Ok(self)
    }

    /// Enable HTTP basic authentication.
    ///
    /// This is available only on Linux and when TLS support is enabled.
    #[cfg(all(
        feature = "tls",
        not(any(target_os = "windows", target_os = "macos", target_os = "ios"))
    ))]
    pub fn basic_auth(self, username: impl std::fmt::Display, password: Option<impl std::fmt::Display>) -> Self {
        let auth = match password {
            Some(password) => format!("{}:{}", username, password),
            None => format!("{}:", username),
        };
        self.header(
            http::header::AUTHORIZATION,
            format!("Basic {}", openssl::base64::encode_block(auth.as_bytes())),
        )
    }

    /// Enable HTTP bearer authentication.
    pub fn bearer_auth(self, token: impl Into<String>) -> Self {
        self.header(http::header::AUTHORIZATION, format!("Bearer {}", token.into()))
    }

    fn body(self, body: impl AsRef<[u8]>) -> RequestBuilder<impl AsRef<[u8]>> {
        RequestBuilder {
            url: self.url,
            method: self.method,
            headers: self.headers,
            body,
            max_redirections: self.max_redirections,
            follow_redirects: self.follow_redirects,
            #[cfg(feature = "charsets")]
            default_charset: self.default_charset,
            #[cfg(feature = "compress")]
            allow_compression: self.allow_compression,
        }
    }

    /// Set the body of this request to be text.
    ///
    /// If the `Content-Type` header is unset, it will be set to `text/plain` and the carset to UTF-8.
    pub fn text(mut self, body: impl AsRef<str>) -> RequestBuilder<impl AsRef<[u8]>> {
        struct Text<B1>(B1);

        impl<B1: AsRef<str>> AsRef<[u8]> for Text<B1> {
            fn as_ref(&self) -> &[u8] {
                self.0.as_ref().as_bytes()
            }
        }

        self.headers
            .entry(http::header::CONTENT_TYPE)
            .or_insert(HeaderValue::from_static("text/plain; charset=utf-8"));
        self.body(Text(body))
    }

    /// Set the body of this request to be bytes.
    ///
    /// If the `Content-Type` header is unset, it will be set to `application/octet-stream`.
    pub fn bytes(mut self, body: impl AsRef<[u8]>) -> RequestBuilder<impl AsRef<[u8]>> {
        self.headers
            .entry(http::header::CONTENT_TYPE)
            .or_insert(HeaderValue::from_static("application/octet-stream"));
        self.body(body)
    }

    /// Set the body of this request to be the JSON representation of the given object.
    ///
    /// If the `Content-Type` header is unset, it will be set to `application/json` and the charset to UTF-8.
    #[cfg(feature = "json")]
    pub fn json<T: serde::Serialize>(mut self, value: &T) -> Result<RequestBuilder<impl AsRef<[u8]>>> {
        let body = serde_json::to_vec(value)?;
        self.headers
            .entry(http::header::CONTENT_TYPE)
            .or_insert(HeaderValue::from_static("application/json; charset=utf-8"));
        Ok(self.body(body))
    }

    /// Set the body of this request to be the URL-encoded representation of the given object.
    ///
    /// If the `Content-Type` header is unset, it will be set to `application/x-www-form-urlencoded`.
    #[cfg(feature = "form")]
    pub fn form<T: serde::Serialize>(mut self, value: &T) -> Result<RequestBuilder<impl AsRef<[u8]>>> {
        let body = serde_urlencoded::to_string(value)?.into_bytes();
        self.headers
            .entry(http::header::CONTENT_TYPE)
            .or_insert(HeaderValue::from_static("application/x-www-form-urlencoded"));
        Ok(self.body(body))
    }

    /// Set the maximum number of redirections this `Request` can perform.
    pub fn max_redirections(mut self, max_redirections: u32) -> Self {
        self.max_redirections = max_redirections;
        self
    }

    /// Sets if this `Request` should follow redirects, 3xx codes.
    ///
    /// This value defaults to true.
    pub fn follow_redirects(mut self, follow_redirects: bool) -> Self {
        self.follow_redirects = follow_redirects;
        self
    }

    /// Set the default charset to use while parsing the response of this `Request`.
    ///
    /// If the response does not say which charset it uses, this charset will be used to decode the request.
    /// This value defaults to `None`, in which case ISO-8859-1 is used.
    #[cfg(feature = "charsets")]
    pub fn default_charset(mut self, default_charset: Option<Charset>) -> Self {
        self.default_charset = default_charset;
        self
    }

    /// Sets if this `Request` will announce that it accepts compression.
    ///
    /// This value defaults to true. Note that this only lets the browser know that this `Request` supports
    /// compression, the server might choose not to compress the content.
    #[cfg(feature = "compress")]
    pub fn allow_compression(mut self, allow_compression: bool) -> Self {
        self.allow_compression = allow_compression;
        self
    }
}

impl<B: AsRef<[u8]>> RequestBuilder<B> {
    /// Create a `PreparedRequest` from this `RequestBuilder`.
    ///
    /// # Panics
    /// Will panic if an error occurs trying to prepare the request. It shouldn't happen.
    pub fn prepare(self) -> PreparedRequest<B> {
        self.try_prepare().expect("failed to prepare request")
    }

    /// Create a `PreparedRequest` from this `RequestBuilder`.
    pub fn try_prepare(self) -> Result<PreparedRequest<B>> {
        let mut prepped = PreparedRequest {
            url: self.url,
            method: self.method,
            headers: self.headers,
            body: self.body,
            max_redirections: self.max_redirections,
            follow_redirects: self.follow_redirects,
            #[cfg(feature = "charsets")]
            default_charset: self.default_charset,
            #[cfg(feature = "compress")]
            allow_compression: self.allow_compression,
        };

        header_insert(&mut prepped.headers, CONNECTION, "close")?;
        prepped.set_compression()?;
        if prepped.has_body() {
            header_insert(&mut prepped.headers, CONTENT_LENGTH, prepped.body.as_ref().len())?;
        }

        header_insert_if_missing(&mut prepped.headers, ACCEPT, "*/*")?;
        header_insert_if_missing(&mut prepped.headers, USER_AGENT, &*DEFAULT_USER_AGENT)?;

        Ok(prepped)
    }

    /// Send this request directly.
    pub fn send(self) -> Result<Response> {
        self.try_prepare()?.send()
    }
}

/// Represents a request that's ready to be sent. You can inspect this object for information about the request.
#[derive(Debug)]
pub struct PreparedRequest<B> {
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: B,
    max_redirections: u32,
    follow_redirects: bool,
    #[cfg(feature = "charsets")]
    pub(crate) default_charset: Option<Charset>,
    #[cfg(feature = "compress")]
    allow_compression: bool,
}

#[cfg(test)]
impl PreparedRequest<Vec<u8>> {
    pub(crate) fn new<U>(method: Method, base_url: U) -> Self
    where
        U: AsRef<str>,
    {
        PreparedRequest {
            url: Url::parse(base_url.as_ref()).unwrap(),
            method,
            headers: HeaderMap::new(),
            body: Vec::new(),
            max_redirections: 5,
            follow_redirects: true,
            #[cfg(feature = "charsets")]
            default_charset: None,
            #[cfg(feature = "compress")]
            allow_compression: true,
        }
    }
}

impl<B> PreparedRequest<B> {
    #[cfg(not(feature = "compress"))]
    fn set_compression(&mut self) -> Result {
        Ok(())
    }

    #[cfg(feature = "compress")]
    fn set_compression(&mut self) -> Result {
        if self.allow_compression {
            header_insert(&mut self.headers, ACCEPT_ENCODING, "gzip, deflate")?;
        }
        Ok(())
    }

    fn base_redirect_url(&self, location: &str, previous_url: &Url) -> Result<Url> {
        match Url::parse(location) {
            Ok(url) => Ok(url),
            Err(url::ParseError::RelativeUrlWithoutBase) => {
                let joined_url = previous_url
                    .join(location)
                    .map_err(|_| InvalidResponseKind::RedirectionUrl)?;

                Ok(joined_url)
            }
            Err(_) => Err(InvalidResponseKind::RedirectionUrl.into()),
        }
    }

    fn write_headers<W>(&self, writer: &mut W) -> Result
    where
        W: Write,
    {
        for (key, value) in self.headers.iter() {
            write!(writer, "{}: ", key.as_str())?;
            writer.write_all(value.as_bytes())?;
            write!(writer, "\r\n")?;
        }
        write!(writer, "\r\n")?;
        Ok(())
    }

    /// Get the URL of this request.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Get the method of this request.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Get the headers of this request.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
}

impl<B: AsRef<[u8]>> PreparedRequest<B> {
    /// Get the body of the request.
    ///
    /// If no body was provided, the slice will be empty.
    pub fn body(&self) -> &[u8] {
        self.body.as_ref()
    }

    fn has_body(&self) -> bool {
        !self.body.as_ref().is_empty() && self.method != Method::TRACE
    }

    fn write_request<W>(&self, writer: W, url: &Url) -> Result
    where
        W: Write,
    {
        let mut writer = BufWriter::new(writer);
        let version = Version::HTTP_11;

        if let Some(query) = url.query() {
            debug!("{} {}?{} {:?}", self.method.as_str(), url.path(), query, version);

            write!(
                writer,
                "{} {}?{} {:?}\r\n",
                self.method.as_str(),
                url.path(),
                query,
                version,
            )?;
        } else {
            debug!("{} {} {:?}", self.method.as_str(), url.path(), version);

            write!(writer, "{} {} {:?}\r\n", self.method.as_str(), url.path(), version)?;
        }

        self.write_headers(&mut writer)?;

        if self.has_body() {
            debug!("writing out body of length {}", self.body.as_ref().len());
            writer.write_all(self.body.as_ref())?;
        }

        writer.flush()?;

        Ok(())
    }

    /// Send this request and wait for the result.
    pub fn send(&mut self) -> Result<Response> {
        let mut url = Cow::Borrowed(&self.url);
        set_host(&mut self.headers, &url)?;

        let mut redirections = 0;

        loop {
            let mut stream = BaseStream::connect(&url)?;
            self.write_request(&mut stream, &url)?;
            let resp = parse_response(stream, self)?;

            debug!("status code {}", resp.status().as_u16());

            if !self.follow_redirects || !resp.status().is_redirection() {
                return Ok(resp);
            }

            redirections += 1;
            if redirections > self.max_redirections {
                return Err(ErrorKind::TooManyRedirections.into());
            }

            // Handle redirect
            let location = resp
                .headers()
                .get(http::header::LOCATION)
                .ok_or(InvalidResponseKind::LocationHeader)?;
            let location = location.to_str().map_err(|_| InvalidResponseKind::LocationHeader)?;

            url = Cow::Owned(self.base_redirect_url(location, &url)?);
            set_host(&mut self.headers, &url)?;

            debug!("redirected to {} giving url {}", location, url);
        }
    }
}

fn set_host(headers: &mut HeaderMap, url: &Url) -> Result {
    let host = url.host_str().ok_or(ErrorKind::InvalidUrlHost)?;
    if let Some(port) = url.port() {
        header_insert(headers, HOST, format!("{}:{}", host, port))?;
    } else {
        header_insert(headers, HOST, host)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_insert_exists() {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("hello"));
        header_insert(&mut headers, USER_AGENT, "world").unwrap();
        assert_eq!(headers[USER_AGENT], "world");
    }

    #[test]
    fn test_header_insert_missing() {
        let mut headers = HeaderMap::new();
        header_insert(&mut headers, USER_AGENT, "world").unwrap();
        assert_eq!(headers[USER_AGENT], "world");
    }

    #[test]
    fn test_header_insert_if_missing_exists() {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("hello"));
        header_insert_if_missing(&mut headers, USER_AGENT, "world").unwrap();
        assert_eq!(headers[USER_AGENT], "hello");
    }

    #[test]
    fn test_header_insert_if_missing_missing() {
        let mut headers = HeaderMap::new();
        header_insert_if_missing(&mut headers, USER_AGENT, "world").unwrap();
        assert_eq!(headers[USER_AGENT], "world");
    }

    #[test]
    fn test_header_append() {
        let mut headers = HeaderMap::new();
        header_append(&mut headers, USER_AGENT, "hello").unwrap();
        header_append(&mut headers, USER_AGENT, "world").unwrap();

        let vals: Vec<_> = headers.get_all(USER_AGENT).into_iter().collect();
        assert_eq!(vals.len(), 2);
        for val in vals {
            assert!(val == "hello" || val == "world");
        }
    }

    #[test]
    fn test_request_builder_param() {
        let prepped = RequestBuilder::new(Method::GET, "http://localhost:1337/foo")
            .param("qux", "baz")
            .prepare();

        assert_eq!(prepped.url().as_str(), "http://localhost:1337/foo?qux=baz");
    }

    #[test]
    fn test_request_builder_params() {
        let prepped = RequestBuilder::new(Method::GET, "http://localhost:1337/foo")
            .params(&[("qux", "baz"), ("foo", "bar")])
            .prepare();

        assert_eq!(prepped.url().as_str(), "http://localhost:1337/foo?qux=baz&foo=bar");
    }

    #[test]
    fn test_request_builder_header_insert() {
        let prepped = RequestBuilder::new(Method::GET, "http://localhost:1337/foo")
            .header("hello", "world")
            .prepare();

        assert_eq!(prepped.headers()["hello"], "world");
    }

    #[test]
    fn test_request_builder_header_append() {
        let prepped = RequestBuilder::new(Method::GET, "http://localhost:1337/foo")
            .header_append("hello", "world")
            .header_append("hello", "!!!")
            .prepare();

        let vals: Vec<_> = prepped.headers().get_all("hello").into_iter().collect();
        assert_eq!(vals.len(), 2);
        for val in vals {
            assert!(val == "world" || val == "!!!");
        }
    }

    fn assert_request_content(
        builder: RequestBuilder,
        status_line: &str,
        mut header_lines: Vec<&str>,
        body_lines: &[&str],
    ) {
        let mut buf = Vec::new();

        let prepped = builder.prepare();
        prepped
            .write_request(&mut buf, prepped.url())
            .expect("error writing request");

        let text = std::str::from_utf8(&buf).expect("cannot decode request as utf-8");
        let lines: Vec<_> = text.lines().collect();

        let req_status_line = lines[0];

        let empty_line_pos = lines
            .iter()
            .position(|l| l.is_empty())
            .expect("no empty line in request");
        let mut req_header_lines = lines[1..empty_line_pos].to_vec();

        let req_body_lines = &lines[empty_line_pos + 1..];

        req_header_lines.sort();
        header_lines.sort();

        assert_eq!(req_status_line, status_line);
        assert_eq!(req_header_lines, header_lines);
        assert_eq!(req_body_lines, body_lines);
    }

    #[test]
    #[cfg(feature = "compress")]
    fn test_request_builder_write_request_no_query() {
        assert_request_content(
            RequestBuilder::new(Method::GET, "http://localhost:1337/foo"),
            "GET /foo HTTP/1.1",
            vec![
                "connection: close",
                "accept-encoding: gzip, deflate",
                "accept: */*",
                &format!("user-agent: {}", *DEFAULT_USER_AGENT),
            ],
            &[],
        );
    }

    #[test]
    #[cfg(feature = "compress")]
    fn test_request_builder_write_request_with_query() {
        assert_request_content(
            RequestBuilder::new(Method::GET, "http://localhost:1337/foo").param("hello", "world"),
            "GET /foo?hello=world HTTP/1.1",
            vec![
                "connection: close",
                "accept-encoding: gzip, deflate",
                "accept: */*",
                &format!("user-agent: {}", *DEFAULT_USER_AGENT),
            ],
            &[],
        );
    }

    #[test]
    fn test_prepare_default_headers() {
        let prepped = RequestBuilder::new(Method::GET, "http://localhost:1337/foo/qux/baz").prepare();
        assert_eq!(prepped.headers()[ACCEPT], "*/*");
        assert_eq!(prepped.headers()[USER_AGENT], *DEFAULT_USER_AGENT);
    }

    #[test]
    fn test_prepare_custom_headers() {
        let prepped = RequestBuilder::new(Method::GET, "http://localhost:1337/foo/qux/baz")
            .header(USER_AGENT, "foobaz")
            .header("Accept", "nothing")
            .prepare();
        assert_eq!(prepped.headers()[ACCEPT], "nothing");
        assert_eq!(prepped.headers()[USER_AGENT], "foobaz");
    }
}
