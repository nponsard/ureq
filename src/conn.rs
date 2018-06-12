use dns_lookup;
use rustls;
use std::io::Write;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::time::Duration;
use stream::Stream;
use url::Url;
use webpki;
use webpki_roots;

const CHUNK_SIZE: usize = 1024 * 1024;

#[derive(Debug, Default, Clone)]
pub struct ConnectionPool {}

impl ConnectionPool {
    fn new() -> Self {
        ConnectionPool {}
    }

    fn connect(
        &mut self,
        request: &Request,
        method: &str,
        url: &Url,
        redirects: u32,
        mut jar: Option<&mut CookieJar>,
        payload: Payload,
    ) -> Result<Response, Error> {
        //

        let hostname = url.host_str().unwrap_or("localhost"); // is localhost a good alternative?
        let is_secure = url.scheme().eq_ignore_ascii_case("https");

        let cookie_headers: Vec<_> = {
            match jar.as_ref() {
                None => vec![],
                Some(jar) => match_cookies(jar, hostname, url.path(), is_secure),
            }
        };
        let headers = request.headers.iter().chain(cookie_headers.iter());

        // open socket
        let mut stream = match url.scheme() {
            "http" => connect_http(request, &url),
            "https" => connect_https(request, &url),
            "test" => connect_test(request, &url),
            _ => Err(Error::UnknownScheme(url.scheme().to_string())),
        }?;

        // send the request start + headers
        let mut prelude: Vec<u8> = vec![];
        write!(prelude, "{} {} HTTP/1.1\r\n", method, url.path())?;
        if !request.has("host") {
            write!(prelude, "Host: {}\r\n", url.host().unwrap())?;
        }
        for header in headers {
            write!(prelude, "{}: {}\r\n", header.name(), header.value())?;
        }
        write!(prelude, "\r\n")?;

        stream.write_all(&mut prelude[..])?;

        // start reading the response to process cookies and redirects.
        let mut resp = Response::from_read(&mut stream);

        // squirrel away cookies
        if let Some(add_jar) = jar.as_mut() {
            for raw_cookie in resp.all("set-cookie").iter() {
                let to_parse = if raw_cookie.to_lowercase().contains("domain=") {
                    raw_cookie.to_string()
                } else {
                    format!("{}; Domain={}", raw_cookie, hostname)
                };
                match Cookie::parse_encoded(&to_parse[..]) {
                    Err(_) => (), // ignore unparseable cookies
                    Ok(mut cookie) => {
                        let cookie = cookie.into_owned();
                        add_jar.add(cookie)
                    }
                }
            }
        }

        // handle redirects
        if resp.redirect() {
            if redirects == 0 {
                return Err(Error::TooManyRedirects);
            }

            // the location header
            let location = resp.header("location");
            if let Some(location) = location {
                // join location header to current url in case it it relative
                let new_url = url.join(location)
                    .map_err(|_| Error::BadUrl(format!("Bad redirection: {}", location)))?;

                // perform the redirect differently depending on 3xx code.
                return match resp.status {
                    301 | 302 | 303 => {
                        send_payload(&request, payload, &mut stream)?;
                        self.connect(request, "GET", &new_url, redirects - 1, jar, Payload::Empty)
                    }
                    307 | 308 | _ => {
                        self.connect(request, method, &new_url, redirects - 1, jar, payload)
                    }
                };
            }
        }

        // send the payload (which can be empty now depending on redirects)
        send_payload(&request, payload, &mut stream)?;

        // since it is not a redirect, give away the incoming stream to the response object
        resp.set_stream(stream);

        // release the response
        Ok(resp)
    }
}

fn connect_http(request: &Request, url: &Url) -> Result<Stream, Error> {
    //
    let hostname = url.host_str().unwrap();
    let port = url.port().unwrap_or(80);

    connect_host(request, hostname, port).map(|tcp| Stream::Http(tcp))
}

fn connect_https(request: &Request, url: &Url) -> Result<Stream, Error> {
    //
    let hostname = url.host_str().unwrap();
    let port = url.port().unwrap_or(443);

    // TODO let user override TLS roots.
    let mut config = rustls::ClientConfig::new();
    config
        .root_store
        .add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);
    let rc_config = Arc::new(config);

    let socket = connect_host(request, hostname, port)?;

    webpki::DNSNameRef::try_from_ascii_str(&hostname)
        .map_err(|_| Error::ConnectionFailed(format!("Invalid TLS name: {}", hostname)))
        .map(|webpki| rustls::ClientSession::new(&rc_config, webpki))
        .map(|client| Stream::Https(client, socket))
}

fn connect_host(request: &Request, hostname: &str, port: u16) -> Result<TcpStream, Error> {
    //
    let ips: Vec<IpAddr> =
        dns_lookup::lookup_host(hostname).map_err(|e| Error::DnsFailed(format!("{}", e)))?;

    if ips.len() == 0 {
        return Err(Error::DnsFailed(format!("No ip address for {}", hostname)));
    }

    // pick first ip, or should we randomize?
    let sock_addr = SocketAddr::new(ips[0], port);

    // connect with a configured timeout.
    let stream = match request.timeout {
        0 => TcpStream::connect(&sock_addr),
        _ => TcpStream::connect_timeout(&sock_addr, Duration::from_millis(request.timeout as u64)),
    }.map_err(|err| Error::ConnectionFailed(format!("{}", err)))?;

    // rust's absurd api returns Err if we set 0.
    if request.timeout_read > 0 {
        stream
            .set_read_timeout(Some(Duration::from_millis(request.timeout_read as u64)))
            .ok();
    }
    if request.timeout_write > 0 {
        stream
            .set_write_timeout(Some(Duration::from_millis(request.timeout_write as u64)))
            .ok();
    }

    Ok(stream)
}

fn send_payload(request: &Request, payload: Payload, stream: &mut Stream) -> IoResult<()> {
    //
    let (size, reader) = payload.into_read();

    let do_chunk = request.header("transfer-encoding")
        // if the user has set an encoding header, obey that.
        .map(|enc| enc.eq_ignore_ascii_case("chunked"))
        // if the content has a size
        .ok_or_else(|| size.
        // or if the user set a content-length header
        or_else(||
            request.header("content-length").map(|len| len.parse::<usize>().unwrap_or(0)))
        // and that size is larger than 1MB, chunk,
        .map(|size| size > CHUNK_SIZE))
        // otherwise, assume chunking since it can be really big.
        .unwrap_or(true);

    if do_chunk {
        pipe(reader, chunked_transfer::Encoder::new(stream))?;
    } else {
        pipe(reader, stream)?;
    }

    Ok(())
}

fn pipe<R, W>(mut reader: R, mut writer: W) -> IoResult<()>
where
    R: Read,
    W: Write,
{
    let mut buf = [0_u8; CHUNK_SIZE];
    loop {
        let len = reader.read(&mut buf)?;
        if len == 0 {
            break;
        }
        writer.write_all(&buf[0..len])?;
    }
    Ok(())
}

// TODO check so cookies can't be set for tld:s
fn match_cookies<'a>(jar: &'a CookieJar, domain: &str, path: &str, is_secure: bool) -> Vec<Header> {
    jar.iter()
        .filter(|c| {
            // if there is a domain, it must be matched. if there is no domain, then ignore cookie
            let domain_ok = c.domain()
                .map(|cdom| domain.contains(cdom))
                .unwrap_or(false);
            // a path must match the beginning of request path. no cookie path, we say is ok. is it?!
            let path_ok = c.path()
                .map(|cpath| path.find(cpath).map(|pos| pos == 0).unwrap_or(false))
                .unwrap_or(true);
            // either the cookie isnt secure, or we're not doing a secure request.
            let secure_ok = !c.secure() || is_secure;

            domain_ok && path_ok && secure_ok
        })
        .map(|c| {
            let name = c.name().to_string();
            let value = c.value().to_string();
            let nameval = Cookie::new(name, value).encoded().to_string();
            let head = format!("Cookie: {}", nameval);
            head.parse::<Header>().ok()
        })
        .filter(|o| o.is_some())
        .map(|o| o.unwrap())
        .collect()
}

#[cfg(not(test))]
fn connect_test(_request: &Request, url: &Url) -> Result<Stream, Error> {
    Err(Error::UnknownScheme(url.scheme().to_string()))
}

#[cfg(test)]
fn connect_test(request: &Request, url: &Url) -> Result<Stream, Error> {
    use test;
    test::resolve_handler(request, url)
}
