use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{timeout, Duration},
};
#[tokio::main]
async fn main() {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper_util::rt::TokioIo;
    for title_case in [false, true] {
        let result = timeout(Duration::from_secs(20), async {
            let connector =
                tokio_native_tls::TlsConnector::from(native_tls::TlsConnector::new().unwrap());
            let stream = connector
                .connect(
                    "github.com",
                    TcpStream::connect("20.205.243.166:443").await?,
                )
                .await
                .map_err(std::io::Error::other)?;
            let stream = tokio::io::BufStream::with_capacity(16384, 16384, stream);
            let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
                .title_case_headers(true)
                .writev(false)
                .handshake(TokioIo::new(stream))
                .await
                .map_err(std::io::Error::other)?;
            tokio::spawn(connection);
            let mut request = hyper::Request::builder()
                .uri("/")
                .header("Host", "github.com")
                .header("User-Agent", "curl/8.13.0")
                .header("Accept", "*/*")
                .body(Full::new(Bytes::new()))
                .unwrap();
            if title_case {
                request.headers_mut().insert(
                    "Connection",
                    hyper::header::HeaderValue::from_static("close"),
                );
            }
            let response = sender
                .send_request(request)
                .await
                .map_err(std::io::Error::other)?;
            let status = response.status();
            let body = response
                .into_body()
                .collect()
                .await
                .map_err(std::io::Error::other)?
                .to_bytes();
            Ok::<_, std::io::Error>((
                status,
                body.len(),
                String::from_utf8_lossy(&body[..body.len().min(40)]).into_owned(),
            ))
        })
        .await;
        println!("hyper title_case={title_case}: {result:?}");
    }
    for sni in [true, false] {
        for alpn in [false, true] {
            let result=timeout(Duration::from_secs(15),async{
            let mut builder=native_tls::TlsConnector::builder();builder.use_sni(sni);if alpn{builder.request_alpns(&["http/1.1"]);}
            let tls=tokio_native_tls::TlsConnector::from(builder.build().unwrap());
            let socket=TcpStream::connect("20.205.243.166:443").await?;let mut stream=tls.connect("github.com",socket).await.map_err(std::io::Error::other)?;
            stream.write_all(b"GET / HTTP/1.1\r\nHost: github.com\r\nUser-Agent: curl/8.13.0\r\nAccept: */*\r\nConnection: close\r\n\r\n").await?;
            let mut header=Vec::new();while header.len()<16384{header.push(stream.read_u8().await?);if header.ends_with(b"\r\n\r\n"){break;}}
            Ok::<_,std::io::Error>(String::from_utf8_lossy(&header).lines().next().unwrap_or("").to_owned())
        }).await;
            println!("sni={sni} alpn={alpn}: {result:?}");
        }
    }
}
