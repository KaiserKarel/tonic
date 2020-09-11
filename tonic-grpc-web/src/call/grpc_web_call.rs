use std::error::Error;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures_core::Stream;
use futures_util::ready;
use http::{HeaderMap, HeaderValue};
use http_body::{Body, SizeHint};
use tonic::Status;

use super::{Direction, Encoding};
use crate::call::Encoding::Base64;

const BUFFER_SIZE: usize = 8 * 1024;
const GRPC_WEB_MARKER: u8 = 0b10000000;

pub struct GrpcWebCall<B> {
    inner: B,
    buf: BytesMut,
    direction: Direction,
    encoding: Encoding,
    poll_trailers: bool,
}

impl<B> GrpcWebCall<B> {
    pub fn new(inner: B, direction: Direction, encoding: Encoding) -> Self {
        GrpcWebCall {
            inner,
            buf: BytesMut::with_capacity(BUFFER_SIZE),
            direction,
            encoding,
            poll_trailers: true,
        }
    }

    fn decode_chunk(&mut self) -> Result<Option<Bytes>, Status> {
        if self.buf.remaining() > 0 && self.buf.remaining() % 4 == 0 {
            let decoded = base64::decode(self.buf.split().freeze()).map_err(internal_error)?;
            Ok(Some(decoded.into()))
        } else {
            Ok(None)
        }
    }

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
        let trailers = self.encode_trailers(trailers);

        let mut frame = Vec::with_capacity(trailers.len() + 5);
        frame.push(GRPC_WEB_MARKER);
        frame.put_u32(trailers.len() as u32);
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
                    None => return Poll::Ready(None),
                }
            },

            Encoding::None => match ready!(Pin::new(&mut self.inner).poll_data(cx)) {
                Some(Err(e)) => Poll::Ready(Some(Err(internal_error(e)))),
                Some(Ok(data)) => Poll::Ready(Some(Ok(data))),
                None => Poll::Ready(None),
            },
        }
    }

    fn poll_encode(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<B::Data, Status>>> {
        if let Some(mut res) = ready!(Pin::new(&mut self.inner).poll_data(cx)) {
            if self.encoding == Base64 {
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
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap<HeaderValue>>, Self::Error>> {
        match self.direction {
            Direction::Request => Pin::new(&mut self.inner)
                .poll_trailers(cx)
                .map_err(internal_error),
            Direction::Response => Poll::Ready(Ok(None)),
        }
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