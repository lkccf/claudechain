//! SOCKS5 客户端握手（RFC 1928 / RFC 1929），在已有连接上执行，用于串联多跳代理。

use std::io;
use std::net::IpAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg.into())
}

/// 通过 `stream` 所连的 SOCKS5 代理，CONNECT 到 host:port。域名交给代理远端解析，本机不做 DNS 查询。
pub async fn connect<S>(stream: &mut S, host: &str, port: u16, auth: Option<(&str, &str)>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let auth = auth.filter(|(u, _)| !u.is_empty());
    let greeting: &[u8] = if auth.is_some() { &[0x05, 0x02, 0x00, 0x02] } else { &[0x05, 0x01, 0x00] };
    stream.write_all(greeting).await?;

    let mut resp = [0u8; 2];
    stream.read_exact(&mut resp).await?;
    if resp[0] != 0x05 {
        return Err(err("对方不是 SOCKS5 代理"));
    }
    match resp[1] {
        0x00 => {}
        0x02 => {
            let (user, pass) = auth.ok_or_else(|| err("代理要求用户名和密码"))?;
            if user.len() > 255 || pass.len() > 255 {
                return Err(err("用户名或密码过长"));
            }
            let mut req = vec![0x01, user.len() as u8];
            req.extend_from_slice(user.as_bytes());
            req.push(pass.len() as u8);
            req.extend_from_slice(pass.as_bytes());
            stream.write_all(&req).await?;
            let mut r = [0u8; 2];
            stream.read_exact(&mut r).await?;
            if r[1] != 0x00 {
                return Err(err("代理用户名或密码错误"));
            }
        }
        _ => return Err(err("代理不支持所需的认证方式")),
    }

    let mut req = vec![0x05, 0x01, 0x00];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            req.push(0x01);
            req.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            req.push(0x04);
            req.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            if host.len() > 255 {
                return Err(err("域名过长"));
            }
            req.push(0x03);
            req.push(host.len() as u8);
            req.extend_from_slice(host.as_bytes());
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        return Err(err(reply_text(head[1])));
    }
    let addr_len = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut l = [0u8; 1];
            stream.read_exact(&mut l).await?;
            l[0] as usize
        }
        _ => return Err(err("代理响应格式错误")),
    };
    let mut rest = vec![0u8; addr_len + 2];
    stream.read_exact(&mut rest).await?;
    Ok(())
}

fn reply_text(code: u8) -> &'static str {
    match code {
        0x01 => "代理服务器内部错误",
        0x02 => "代理规则不允许该连接",
        0x03 => "代理报告网络不可达",
        0x04 => "代理报告主机不可达",
        0x05 => "目标拒绝连接",
        0x06 => "代理连接超时",
        0x07 => "代理不支持该命令",
        0x08 => "代理不支持该地址类型",
        _ => "代理返回未知错误",
    }
}
