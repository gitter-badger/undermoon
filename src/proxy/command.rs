use super::slowlog::Slowlog;
use crate::common::utils::byte_to_uppercase;
use crate::protocol::{RespPacket, RespSlice, RespVec};
use arrayvec::ArrayVec;
use futures::channel::oneshot;
use futures::task::{Context, Poll};
use futures::Future;
use pin_project::pin_project;
use std::convert::identity;
use std::error::Error;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::result::Result;
use std::str;

const MAX_COMMAND_NAME_LENGTH: usize = 64;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum CmdType {
    Ping,
    Info,
    Auth,
    Quit,
    Echo,
    Select,
    Others,
    Invalid,
    UmCtl,
    Cluster,
    Config,
    Command,
}

impl CmdType {
    fn from_cmd_name(cmd_name: &[u8]) -> Self {
        let mut stack_cmd_name = ArrayVec::<[u8; MAX_COMMAND_NAME_LENGTH]>::new();
        for b in cmd_name {
            if let Err(err) = stack_cmd_name.try_push(byte_to_uppercase(*b)) {
                error!("Unexpected long command name: {:?} {:?}", cmd_name, err);
                return CmdType::Others;
            }
        }
        // The underlying `deref` will take the real length intead of the whole MAX_COMMAND_NAME_LENGTH array;
        let cmd_name: &[u8] = &stack_cmd_name;

        match cmd_name {
            b"PING" => CmdType::Ping,
            b"INFO" => CmdType::Info,
            b"AUTH" => CmdType::Auth,
            b"QUIT" => CmdType::Quit,
            b"ECHO" => CmdType::Echo,
            b"SELECT" => CmdType::Select,
            b"UMCTL" => CmdType::UmCtl,
            b"CLUSTER" => CmdType::Cluster,
            b"CONFIG" => CmdType::Config,
            b"COMMAND" => CmdType::Command,
            _ => CmdType::Others,
        }
    }

    pub fn from_packet(packet: &RespPacket) -> Self {
        let cmd_name = match packet.get_array_element(0) {
            Some(cmd_name) => cmd_name,
            None => return CmdType::Invalid,
        };

        CmdType::from_cmd_name(cmd_name)
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum DataCmdType {
    APPEND,
    BITCOUNT,
    BITFIELD,
    BITOP,
    BITPOS,
    DECR,
    DECRBY,
    GET,
    GETBIT,
    GETRANGE,
    GETSET,
    INCR,
    INCRBY,
    INCRBYFLOAT,
    MGET,
    MSET,
    MSETNX,
    PSETEX,
    SET,
    SETBIT,
    SETEX,
    SETNX,
    SETRANGE,
    STRLEN,
    EVAL,
    EVALSHA,
    DEL,
    EXISTS,
    Others,
}

impl DataCmdType {
    fn from_cmd_name(cmd_name: &[u8]) -> Self {
        let mut stack_cmd_name = ArrayVec::<[u8; MAX_COMMAND_NAME_LENGTH]>::new();
        for b in cmd_name {
            if let Err(err) = stack_cmd_name.try_push(byte_to_uppercase(*b)) {
                error!(
                    "Unexpected long data command name: {:?} {:?}",
                    cmd_name, err
                );
                return DataCmdType::Others;
            }
        }
        // The underlying `deref` will take the real length intead of the whole MAX_COMMAND_NAME_LENGTH array;
        let cmd_name: &[u8] = &stack_cmd_name;

        match cmd_name {
            b"APPEND" => DataCmdType::APPEND,
            b"BITCOUNT" => DataCmdType::BITCOUNT,
            b"BITFIELD" => DataCmdType::BITFIELD,
            b"BITOP" => DataCmdType::BITOP,
            b"BITPOS" => DataCmdType::BITPOS,
            b"DECR" => DataCmdType::DECR,
            b"DECRBY" => DataCmdType::DECRBY,
            b"GET" => DataCmdType::GET,
            b"GETBIT" => DataCmdType::GETBIT,
            b"GETRANGE" => DataCmdType::GETRANGE,
            b"GETSET" => DataCmdType::GETSET,
            b"INCR" => DataCmdType::INCR,
            b"INCRBY" => DataCmdType::INCRBY,
            b"INCRBYFLOAT" => DataCmdType::INCRBYFLOAT,
            b"MGET" => DataCmdType::MGET,
            b"MSET" => DataCmdType::MSET,
            b"MSETNX" => DataCmdType::MSETNX,
            b"PSETEX" => DataCmdType::PSETEX,
            b"SET" => DataCmdType::SET,
            b"SETBIT" => DataCmdType::SETBIT,
            b"SETEX" => DataCmdType::SETEX,
            b"SETNX" => DataCmdType::SETNX,
            b"SETRANGE" => DataCmdType::SETRANGE,
            b"STRLEN" => DataCmdType::STRLEN,
            b"EVAL" => DataCmdType::EVAL,
            b"EVALSHA" => DataCmdType::EVALSHA,
            b"DEL" => DataCmdType::DEL,
            b"EXISTS" => DataCmdType::EXISTS,
            _ => DataCmdType::Others,
        }
    }

    pub fn from_packet(packet: &RespPacket) -> Self {
        let cmd_name = match packet.get_array_element(0) {
            Some(cmd_name) => cmd_name,
            None => return DataCmdType::Others,
        };

        DataCmdType::from_cmd_name(cmd_name)
    }
}

#[derive(Debug)]
pub struct Command {
    request: Box<RespPacket>,
    cmd_type: CmdType,
    data_cmd_type: DataCmdType,
}

impl Command {
    pub fn new(request: Box<RespPacket>) -> Self {
        let cmd_type = CmdType::from_packet(&request);
        let data_cmd_type = DataCmdType::from_packet(&request);
        Command {
            request,
            cmd_type,
            data_cmd_type,
        }
    }

    pub fn into_packet(self) -> Box<RespPacket> {
        self.request
    }

    pub fn get_packet(&self) -> RespPacket {
        self.request.as_ref().clone()
    }

    pub fn get_resp_slice(&self) -> RespSlice {
        self.request.to_resp_slice()
    }

    pub fn get_command_element(&self, index: usize) -> Option<&[u8]> {
        self.request.get_array_element(index)
    }

    pub fn get_command_name(&self) -> Option<&str> {
        self.request.get_command_name()
    }

    pub fn change_element(&mut self, index: usize, data: Vec<u8>) -> bool {
        self.request.change_bulk_array_element(index, data)
    }

    pub fn get_type(&self) -> CmdType {
        self.cmd_type
    }

    pub fn get_data_cmd_type(&self) -> DataCmdType {
        self.data_cmd_type
    }

    pub fn get_key(&self) -> Option<&[u8]> {
        match self.data_cmd_type {
            DataCmdType::EVAL | DataCmdType::EVALSHA => self.get_command_element(3),
            _ => self.get_command_element(1),
        }
    }
}

pub struct TaskReply {
    request: Box<RespPacket>,
    packet: Box<RespPacket>,
    slowlog: Slowlog,
}

impl TaskReply {
    pub fn new(request: Box<RespPacket>, packet: Box<RespPacket>, slowlog: Slowlog) -> Self {
        Self {
            request,
            packet,
            slowlog,
        }
    }

    pub fn into_inner(self) -> (Box<RespPacket>, Box<RespPacket>, Slowlog) {
        let Self {
            request,
            packet,
            slowlog,
        } = self;
        (request, packet, slowlog)
    }

    pub fn into_resp_vec(self) -> RespVec {
        let (_, packet, _) = self.into_inner();
        packet.into_resp_vec()
    }
}

pub type CommandResult<T> = Result<Box<T>, CommandError>;
pub type TaskResult = Result<Box<TaskReply>, CommandError>;

pub fn new_command_pair() -> (CmdReplySender, CmdReplyReceiver) {
    let (s, r) = oneshot::channel::<TaskResult>();
    let reply_sender = CmdReplySender {
        reply_sender: Some(s),
    };
    let reply_receiver = CmdReplyReceiver { reply_receiver: r };
    (reply_sender, reply_receiver)
}

pub struct CmdReplySender {
    reply_sender: Option<oneshot::Sender<TaskResult>>,
}

impl fmt::Debug for CmdReplySender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "CmdReplySender")
    }
}

impl CmdReplySender {
    pub fn send(&mut self, res: TaskResult) -> Result<(), CommandError> {
        // Must not send twice.
        match self.try_send(res) {
            Some(res) => res,
            None => {
                error!("unexpected send again");
                Err(CommandError::InnerError)
            }
        }
    }

    fn try_send(&mut self, res: TaskResult) -> Option<Result<(), CommandError>> {
        // Must not send twice.
        match self.reply_sender.take() {
            Some(reply_sender) => Some(reply_sender.send(res).map_err(|_| CommandError::Canceled)),
            None => None,
        }
    }
}

// Make sure that result will always be sent back
impl Drop for CmdReplySender {
    fn drop(&mut self) {
        self.try_send(Err(CommandError::Dropped));
    }
}

#[pin_project]
pub struct CmdReplyReceiver {
    #[pin]
    reply_receiver: oneshot::Receiver<TaskResult>,
}

impl Future for CmdReplyReceiver {
    type Output = TaskResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().reply_receiver.poll(cx).map(|result| {
            result
                .map_err(|_| CommandError::Canceled)
                .and_then(identity)
        })
    }
}

#[derive(Debug)]
pub enum CommandError {
    Io(io::Error),
    UnexpectedResponse,
    Dropped,
    Canceled,
    InnerError,
}

impl Clone for CommandError {
    fn clone(&self) -> Self {
        match self {
            Self::Io(ioerr) => {
                let err = io::Error::from(ioerr.kind());
                Self::Io(err)
            }
            Self::UnexpectedResponse => Self::UnexpectedResponse,
            Self::Dropped => Self::Dropped,
            Self::Canceled => Self::Canceled,
            Self::InnerError => Self::InnerError,
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for CommandError {
    fn description(&self) -> &str {
        "command error"
    }

    fn cause(&self) -> Option<&dyn Error> {
        match self {
            CommandError::Io(err) => Some(err),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cmd_type() {
        assert_eq!(CmdType::from_cmd_name(b"pInG"), CmdType::Ping);
        assert_eq!(CmdType::from_cmd_name(b"get"), CmdType::Others);
    }

    #[test]
    fn test_parse_data_cmd_type() {
        assert_eq!(DataCmdType::from_cmd_name(b"aPPend"), DataCmdType::APPEND);
        assert_eq!(DataCmdType::from_cmd_name(b"get"), DataCmdType::GET);
        assert_eq!(DataCmdType::from_cmd_name(b"eVaL"), DataCmdType::EVAL);
        assert_eq!(DataCmdType::from_cmd_name(b"HMGET"), DataCmdType::Others);
    }
}
