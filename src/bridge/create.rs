//! This module contains adaptations of the functions found in
//! https://github.com/KillTheMule/nvim-rs/blob/master/src/create/tokio.rs

use std::{
    io::{self, Error, ErrorKind},
    process::Stdio,
};

use tokio::{
    io::split,
    net::{TcpStream, ToSocketAddrs},
    process::Command,
    spawn,
    task::JoinHandle,
};

use nvim_rs::compat::tokio::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use nvim_rs::{error::LoopError, neovim::Neovim, Handler};

use crate::bridge::{TxWrapper, WrapTx};

/// Connect to a neovim instance via tcp
pub async fn new_tcp<A, H>(
    addr: A,
    handler: H,
) -> io::Result<(
    Neovim<Compat<TxWrapper>>,
    JoinHandle<Result<(), Box<LoopError>>>,
)>
where
    A: ToSocketAddrs,
    H: Handler<Writer = Compat<TxWrapper>>,
{
    let stream = TcpStream::connect(addr).await?;
    let (reader, writer) = split(stream);
    let (neovim, io) = Neovim::<Compat<TxWrapper>>::new(
        reader.compat_read(),
        writer.wrap_tx().compat_write(),
        handler,
    );
    let io_handle = spawn(io);

    Ok((neovim, io_handle))
}

/// Connect to a neovim instance by spawning a new one
///
/// stdin/stdout will be rewritten to `Stdio::piped()`
pub async fn new_child_cmd<H>(
    cmd: &mut Command,
    handler: H,
) -> io::Result<(
    Neovim<Compat<TxWrapper>>,
    JoinHandle<Result<(), Box<LoopError>>>,
)>
where
    H: Handler<Writer = Compat<TxWrapper>>,
{
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::new(ErrorKind::Other, "Can't open stdout"))?
        .compat_read();
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::new(ErrorKind::Other, "Can't open stdin"))?
        .wrap_tx()
        .compat_write();

    let (neovim, io) = Neovim::<Compat<TxWrapper>>::new(stdout, stdin, handler);
    let io_handle = spawn(io);

    Ok((neovim, io_handle))
}
