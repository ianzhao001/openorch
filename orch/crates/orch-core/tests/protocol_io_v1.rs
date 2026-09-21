//! B366 shared line bounds. Mutate frame caps, per-line reset and incomplete flush refusal independently.
#![cfg(feature="protocol-io")]
use orch_core::protocol_io::{BoundedReader,BoundedWriter};
use tokio::io::{AsyncReadExt,AsyncWriteExt};
#[test]
fn input_frames_are_individually_bounded() {
 tokio::runtime::Runtime::new().unwrap().block_on(async {
  let mut ok=BoundedReader::new(std::io::Cursor::new(b"a\nb\nc\n".to_vec()),2);
  let mut bytes=Vec::new();ok.read_to_end(&mut bytes).await.unwrap();assert_eq!(bytes,b"a\nb\nc\n");
  let mut large=BoundedReader::new(std::io::Cursor::new(b"12345\n".to_vec()),4);
  assert!(large.read_to_end(&mut Vec::new()).await.is_err());
 });
}
#[test]
fn oversized_output_emits_no_prefix() {
 tokio::runtime::Runtime::new().unwrap().block_on(async {
  let mut out=BoundedWriter::new(Vec::<u8>::new(),8);
  assert!(out.write_all(b"123456789\n").await.is_err());assert!(out.get_ref().is_empty());
 });
}
#[test]
fn partial_frame_flush_cannot_report_success() {
 tokio::runtime::Runtime::new().unwrap().block_on(async {
  let mut out=BoundedWriter::new(Vec::<u8>::new(),128);
  out.write_all(b"{\"partial\":").await.unwrap();assert!(out.flush().await.is_err());assert!(out.get_ref().is_empty());
 });
}
#[test]
fn multiple_complete_frames_preserve_exact_bytes() {
 tokio::runtime::Runtime::new().unwrap().block_on(async {
  let mut out=BoundedWriter::new(Vec::<u8>::new(),8);
  out.write_all(b"{\"a\":1}\n{\"b\":2}\n").await.unwrap();out.flush().await.unwrap();assert_eq!(out.get_ref(),b"{\"a\":1}\n{\"b\":2}\n");
 });
}
