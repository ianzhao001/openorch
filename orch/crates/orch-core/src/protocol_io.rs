//! Bounded newline streams shared by protocol binaries, without platform-specific code.
#![deny(missing_docs)]
use std::{io,pin::Pin,task::{Context,Poll}};
use tokio::io::{AsyncRead,AsyncWrite,ReadBuf};
/// An async reader with per-frame and optional lifetime byte ceilings.
pub struct BoundedReader<R> { inner:R, frame:usize, limit:usize, total:usize, total_limit:Option<usize> }
impl<R> BoundedReader<R> {
    /// Bound each newline-terminated frame independently; EOF is left to the codec.
    pub fn new(inner:R,limit:usize)->Self {Self{inner,frame:0,limit,total:0,total_limit:None}}
    /// Additionally reject a finite transport that exceeds this total byte budget.
    pub fn with_total_limit(mut self,limit:usize)->Self {self.total_limit=Some(limit);self}
}
impl<R:AsyncRead+Unpin> AsyncRead for BoundedReader<R> {
    fn poll_read(mut self:Pin<&mut Self>,cx:&mut Context<'_>,out:&mut ReadBuf<'_>)->Poll<io::Result<()>> {
        let mut bytes=[0u8;8192];let n=out.remaining().min(bytes.len());if n==0{return Poll::Ready(Ok(()));}
        let mut input=ReadBuf::new(&mut bytes[..n]);
        match Pin::new(&mut self.inner).poll_read(cx,&mut input) {
            Poll::Ready(Ok(()))=>{
                for byte in input.filled() {
                    self.frame=self.frame.saturating_add(1);self.total=self.total.saturating_add(1);
                    if self.frame>self.limit || self.total_limit.is_some_and(|limit|self.total>limit) {
                        return Poll::Ready(Err(io::Error::new(io::ErrorKind::InvalidData,"protocol input exceeds byte budget")));
                    }
                    if *byte==b'\n' {self.frame=0;}
                }
                out.put_slice(input.filled());Poll::Ready(Ok(()))
            },other=>other,
        }
    }
}
/// An async writer that checks a complete frame before emitting any of its bytes.
/// A flush of an incomplete frame is an error, never a successful partial write.
pub struct BoundedWriter<W> { inner:W, frame:Vec<u8>, written:usize, limit:usize }
impl<W> BoundedWriter<W> {
    /// Create a writer with a per-line byte limit, including its final newline.
    pub fn new(inner:W,limit:usize)->Self {Self{inner,frame:Vec::new(),written:0,limit}}
    /// Inspect the underlying writer without altering framing state.
    pub fn get_ref(&self)->&W {&self.inner}
}
impl<W:AsyncWrite+Unpin> BoundedWriter<W> {
    fn flush_frame(&mut self,cx:&mut Context<'_>)->Poll<io::Result<()>> {
        while self.written<self.frame.len() {
            match Pin::new(&mut self.inner).poll_write(cx,&self.frame[self.written..]) {
                Poll::Pending=>return Poll::Pending,
                Poll::Ready(Err(error))=>return Poll::Ready(Err(error)),
                Poll::Ready(Ok(0))=>return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                Poll::Ready(Ok(n))=>self.written+=n,
            }
        }
        self.frame.clear();self.written=0;Poll::Ready(Ok(()))
    }
}
impl<W:AsyncWrite+Unpin> AsyncWrite for BoundedWriter<W> {
    fn poll_write(mut self:Pin<&mut Self>,cx:&mut Context<'_>,bytes:&[u8])->Poll<io::Result<usize>> {
        if self.frame.last()==Some(&b'\n') {
            match self.flush_frame(cx) {Poll::Ready(Ok(()))=>{},Poll::Ready(Err(e))=>return Poll::Ready(Err(e)),Poll::Pending=>return Poll::Pending}
        }
        let n=bytes.iter().position(|b|*b==b'\n').map(|i|i+1).unwrap_or(bytes.len());
        if self.frame.len().saturating_add(n)>self.limit {return Poll::Ready(Err(io::Error::new(io::ErrorKind::InvalidData,"protocol output exceeds frame budget")));}
        self.frame.extend_from_slice(&bytes[..n]);Poll::Ready(Ok(n))
    }
    fn poll_flush(mut self:Pin<&mut Self>,cx:&mut Context<'_>)->Poll<io::Result<()>> {
        if !self.frame.is_empty() && self.frame.last()!=Some(&b'\n') {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::InvalidData,"incomplete protocol output frame")));
        }
        match self.flush_frame(cx) {Poll::Ready(Ok(()))=>{},other=>return other}
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self:Pin<&mut Self>,cx:&mut Context<'_>)->Poll<io::Result<()>> {
        match self.as_mut().poll_flush(cx) {Poll::Ready(Ok(()))=>{},other=>return other}
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
