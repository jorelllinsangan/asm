//! Length-prefixed JSON framing over any async byte stream.

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame we will accept, guarding against a corrupt length prefix.
const MAX_FRAME: usize = 64 * 1024 * 1024;

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(msg)?;
    let len = u32::try_from(bytes.len())?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// One frame read off the wire.
///
/// The two failure modes are deliberately kept apart. An `Err` from
/// [`read_frame`] means the stream itself is unusable (peer gone, or a length
/// prefix we can't trust) and the connection must end. `Undecodable` means the
/// frame arrived intact but didn't match `T` — which is what a peer speaking a
/// *newer* protocol looks like. That is recoverable: the length prefix told us
/// exactly how many bytes to consume, so the stream is still in sync and the
/// only right move is to skip the frame and carry on.
///
/// Conflating the two is why a single unknown request used to tear down the
/// whole connection — and, on the client side, take the TUI down with it.
pub enum Frame<T> {
    Msg(T),
    Undecodable(String),
}

pub async fn read_frame<R, T>(r: &mut R) -> Result<Frame<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        anyhow::bail!("frame too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(match serde_json::from_slice(&buf) {
        Ok(msg) => Frame::Msg(msg),
        Err(e) => Frame::Undecodable(e.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    enum Msg {
        Known(u32),
    }

    /// Frame `value` the way `write_frame` would, without needing a socket.
    fn framed<T: Serialize>(value: &T) -> Vec<u8> {
        let body = serde_json::to_vec(value).unwrap();
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&body);
        out
    }

    fn read_one<T: DeserializeOwned>(bytes: &mut &[u8]) -> Result<Frame<T>> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(read_frame::<_, T>(bytes))
    }

    #[test]
    fn a_known_message_round_trips() {
        let buf = framed(&Msg::Known(7));
        let mut r = buf.as_slice();
        match read_one::<Msg>(&mut r).unwrap() {
            Frame::Msg(m) => assert_eq!(m, Msg::Known(7)),
            Frame::Undecodable(e) => panic!("unexpected decode failure: {e}"),
        }
    }

    #[test]
    fn an_unknown_variant_is_undecodable_not_an_error() {
        // What a newer peer's request looks like to an older build.
        let buf = framed(&serde_json::json!({ "FromTheFuture": { "x": 1 } }));
        let mut r = buf.as_slice();
        match read_one::<Msg>(&mut r).unwrap() {
            Frame::Undecodable(_) => {}
            Frame::Msg(m) => panic!("should not have decoded: {m:?}"),
        }
    }

    #[test]
    fn an_undecodable_frame_leaves_the_stream_in_sync() {
        // The whole point: skipping a frame we don't understand must not
        // desynchronise the connection, or "recoverable" would be a lie.
        let mut buf = framed(&serde_json::json!({ "FromTheFuture": { "x": 1 } }));
        buf.extend_from_slice(&framed(&Msg::Known(42)));
        let mut r = buf.as_slice();

        assert!(matches!(
            read_one::<Msg>(&mut r).unwrap(),
            Frame::Undecodable(_)
        ));
        match read_one::<Msg>(&mut r).unwrap() {
            Frame::Msg(m) => assert_eq!(m, Msg::Known(42), "next frame must still be readable"),
            Frame::Undecodable(e) => panic!("stream desynchronised: {e}"),
        }
    }

    #[test]
    fn a_truncated_body_is_a_hard_error() {
        // A short read means the peer is gone — that must stay fatal, so the
        // recoverable path can't swallow a dead connection.
        let mut buf = framed(&Msg::Known(1));
        buf.truncate(buf.len() - 2);
        let mut r = buf.as_slice();
        assert!(read_one::<Msg>(&mut r).is_err());
    }

    #[test]
    fn an_oversized_length_prefix_is_rejected() {
        let buf = [0xFFu8, 0xFF, 0xFF, 0xFF];
        let mut r = buf.as_slice();
        assert!(read_one::<Msg>(&mut r).is_err());
    }
}
