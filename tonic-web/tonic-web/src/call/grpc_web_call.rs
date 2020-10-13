use super::{Direction, Encoding};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures_core::{ready, Stream};
use http::{HeaderMap, HeaderValue};
use http_body::{Body, SizeHint};
use std::{
    error::Error,
    pin::Pin,
    task::{Context, Poll},
};
use tonic::Status;

const BUFFER_SIZE: usize = 2 * 1024;

// 8th (MSB) bit of the 1st gRPC frame byte
// denotes an uncompressed trailer (as part of the body)
const GRPC_WEB_TRAILERS_BIT: u8 = 0b10000000;

pub(crate) struct GrpcWebCall<B> {
    inner: B,
    buf: BytesMut,
    direction: Direction,
    encoding: Encoding,
    poll_trailers: bool,
}

impl<B> GrpcWebCall<B> {
    pub(crate) fn request(inner: B, encoding: Encoding) -> Self {
        Self::new(inner, Direction::Request, encoding)
    }

    pub(crate) fn response(inner: B, encoding: Encoding) -> Self {
        Self::new(inner, Direction::Response, encoding)
    }

    fn new(inner: B, direction: Direction, encoding: Encoding) -> Self {
        GrpcWebCall {
            inner,
            buf: BytesMut::with_capacity(match (direction, encoding) {
                (Direction::Response, Encoding::Base64) => BUFFER_SIZE,
                _ => 0,
            }),
            direction,
            encoding,
            poll_trailers: true,
        }
    }

    #[inline]
    fn max_decodable(&self) -> usize {
        (self.buf.len() / 4) * 4
    }

    fn decode_chunk(&mut self) -> Result<Option<Bytes>, Status> {
        // not enough bytes to decode
        if self.buf.is_empty() || self.buf.len() < 4 {
            return Ok(None);
        }

        // Split `buf` at the largest index that is multiple of 4. Decode the
        // returned `Bytes`, keeping the rest for the next attempt to decode.
        base64::decode(self.buf.split_to(self.max_decodable()).freeze())
            .map(|decoded| Some(Bytes::from(decoded)))
            .map_err(internal_error)
    }

    // Key-value pairs encoded as a HTTP/1 headers block (without the terminating newline)
    fn encode_trailers(&self, trailers: HeaderMap) -> Vec<u8> {
        trailers.iter().fold(Vec::new(), |mut acc, (key, value)| {
            acc.put_slice(key.as_ref());
            acc.push(b':');
            acc.put_slice(value.as_bytes());
            acc.put_slice(b"\r\n");
            acc
        })
    }

    fn make_trailers_frame(&self, trailers: HeaderMap) -> Vec<u8> {
        const HEADER_SIZE: usize = 5;

        let trailers = self.encode_trailers(trailers);
        let len = trailers.len();
        assert!(len <= std::u32::MAX as usize);

        let mut frame = Vec::with_capacity(len + HEADER_SIZE);
        frame.push(GRPC_WEB_TRAILERS_BIT);
        frame.put_u32(len as u32);
        frame.extend(trailers);

        frame
    }
}

impl<B> GrpcWebCall<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Error,
{
    fn poll_decode(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<B::Data, Status>>> {
        match self.encoding {
            Encoding::Base64 => loop {
                if let Some(bytes) = self.decode_chunk()? {
                    return Poll::Ready(Some(Ok(bytes)));
                }

                match ready!(Pin::new(&mut self.inner).poll_data(cx)) {
                    Some(Ok(data)) => self.buf.put(data),
                    Some(Err(e)) => return Poll::Ready(Some(Err(internal_error(e)))),
                    None => {
                        return if self.buf.has_remaining() {
                            Poll::Ready(Some(Err(internal_error("malformed base64 request"))))
                        } else {
                            Poll::Ready(None)
                        }
                    }
                }
            },

            Encoding::None => match ready!(Pin::new(&mut self.inner).poll_data(cx)) {
                Some(res) => Poll::Ready(Some(res.map_err(internal_error))),
                None => Poll::Ready(None),
            },
        }
    }

    fn poll_encode(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<B::Data, Status>>> {
        if let Some(mut res) = ready!(Pin::new(&mut self.inner).poll_data(cx)) {
            if self.encoding == Encoding::Base64 {
                res = res.map(|b| base64::encode(b).into())
            }

            return Poll::Ready(Some(res.map_err(internal_error)));
        }

        // this flag is needed because the inner stream never
        // returns Poll::Ready(None) when polled for trailers
        if self.poll_trailers {
            return match ready!(Pin::new(&mut self.inner).poll_trailers(cx)) {
                Ok(Some(map)) => {
                    let mut frame = self.make_trailers_frame(map);

                    if self.encoding == Encoding::Base64 {
                        frame = base64::encode(frame).into_bytes();
                    }

                    self.poll_trailers = false;
                    Poll::Ready(Some(Ok(frame.into())))
                }
                Ok(None) => Poll::Ready(None),
                Err(e) => Poll::Ready(Some(Err(internal_error(e)))),
            };
        }

        Poll::Ready(None)
    }
}

impl<B> Body for GrpcWebCall<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Error,
{
    type Data = Bytes;
    type Error = Status;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.direction {
            Direction::Request => self.poll_decode(cx),
            Direction::Response => self.poll_encode(cx),
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap<HeaderValue>>, Self::Error>> {
        Poll::Ready(Ok(None))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Stream for GrpcWebCall<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Error,
{
    type Item = Result<Bytes, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Body::poll_data(self, cx)
    }
}

fn internal_error(e: impl std::fmt::Display) -> Status {
    Status::internal(e.to_string())
}
