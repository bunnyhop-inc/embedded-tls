use heapless::{consts::*, ArrayLength, Vec};
use p256::ecdh::EphemeralSecret;
use p256::{EncodedPoint, NistP256};
use rand_core::{CryptoRng, RngCore};

use crate::cipher_suites::CipherSuite;
//use p256::elliptic_curve::AffinePoint;
use crate::config::{Config, TlsCipherSuite};
use crate::content_types::ContentType;
use crate::content_types::ContentType::Handshake;
use crate::extensions::ClientExtension;
use crate::extensions::ExtensionType::SupportedVersions;
use crate::handshake::certificate::Certificate;
use crate::handshake::certificate_verify::CertificateVerify;
use crate::handshake::client_hello::ClientHello;
use crate::handshake::encrypted_extensions::EncryptedExtensions;
use crate::handshake::finished::Finished;
use crate::handshake::server_hello::ServerHello;
use crate::max_fragment_length::MaxFragmentLength;
use crate::named_groups::NamedGroup;
use crate::parse_buffer::ParseBuffer;
use crate::signature_schemes::SignatureScheme;
use crate::supported_versions::{ProtocolVersion, TLS13};
use crate::TlsError::InvalidHandshake;
use crate::{AsyncRead, AsyncWrite, TlsError};
use core::fmt::{Debug, Formatter};
use core::ops::Range;
use sha2::Digest;

pub mod certificate;
pub mod certificate_verify;
pub mod client_hello;
pub mod encrypted_extensions;
pub mod finished;
pub mod server_hello;

const LEGACY_VERSION: u16 = 0x0303;

type Random = [u8; 32];

const HELLO_RETRY_REQUEST_RANDOM: [u8; 32] = [
    0xCF, 0x21, 0xAD, 0x74, 0xE5, 0x9A, 0x61, 0x11, 0xBE, 0x1D, 0x8C, 0x02, 0x1E, 0x65, 0xB8, 0x91,
    0xC2, 0xA2, 0x11, 0x16, 0x7A, 0xBB, 0x8C, 0x5E, 0x07, 0x9E, 0x09, 0xE2, 0xC8, 0xA8, 0x33, 0x9C,
];

#[derive(Debug, Copy, Clone)]
pub enum HandshakeType {
    ClientHello = 1,
    ServerHello = 2,
    NewSessionTicket = 4,
    EndOfEarlyData = 5,
    EncryptedExtensions = 8,
    Certificate = 11,
    CertificateRequest = 13,
    CertificateVerify = 15,
    Finished = 20,
    KeyUpdate = 24,
    MessageHash = 254,
}

impl HandshakeType {
    pub fn of(num: u8) -> Option<Self> {
        info!("find handshake type of {}", num);
        match num {
            1 => Some(HandshakeType::ClientHello),
            2 => Some(HandshakeType::ServerHello),
            4 => Some(HandshakeType::NewSessionTicket),
            5 => Some(HandshakeType::EndOfEarlyData),
            8 => Some(HandshakeType::EncryptedExtensions),
            11 => Some(HandshakeType::Certificate),
            13 => Some(HandshakeType::CertificateRequest),
            15 => Some(HandshakeType::CertificateVerify),
            20 => Some(HandshakeType::Finished),
            24 => Some(HandshakeType::KeyUpdate),
            254 => Some(HandshakeType::MessageHash),
            _ => None,
        }
    }
}

pub enum ClientHandshake<'config, RNG, CipherSuite>
where
    RNG: CryptoRng + RngCore + Copy,
    CipherSuite: TlsCipherSuite,
{
    ClientHello(ClientHello<'config, RNG, CipherSuite>),
    Finished(Finished<<CipherSuite::Hash as Digest>::OutputSize>),
}

impl<'config, RNG, CipherSuite> ClientHandshake<'config, RNG, CipherSuite>
where
    RNG: CryptoRng + RngCore + Copy,
    CipherSuite: TlsCipherSuite,
{
    pub fn encode<O: ArrayLength<u8>>(
        &self,
        buf: &mut Vec<u8, O>,
    ) -> Result<Range<usize>, TlsError> {
        let content_marker = buf.len();
        match self {
            ClientHandshake::ClientHello(_) => {
                buf.push(HandshakeType::ClientHello as u8);
            }
            ClientHandshake::Finished(_) => {
                buf.push(HandshakeType::Finished as u8);
            }
        }

        let content_length_marker = buf.len();
        buf.push(0);
        buf.push(0);
        buf.push(0);
        match self {
            ClientHandshake::ClientHello(inner) => inner.encode(buf)?,
            ClientHandshake::Finished(inner) => inner.encode(buf)?,
        }
        let content_length = (buf.len() as u32 - content_length_marker as u32) - 3;

        buf[content_length_marker] = content_length.to_be_bytes()[1];
        buf[content_length_marker + 1] = content_length.to_be_bytes()[2];
        buf[content_length_marker + 2] = content_length.to_be_bytes()[3];

        info!("hash [{:x?}]", &buf[content_marker..]);
        //digest.update(&buf[content_marker..]);

        Ok(content_marker..buf.len())
    }
}

pub enum ServerHandshake<N: ArrayLength<u8>> {
    ServerHello(ServerHello),
    EncryptedExtensions(EncryptedExtensions),
    Certificate(Certificate),
    CertificateVerify(CertificateVerify),
    Finished(Finished<N>),
}

impl<N: ArrayLength<u8>> Debug for ServerHandshake<N> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            ServerHandshake::ServerHello(inner) => Debug::fmt(inner, f),
            ServerHandshake::EncryptedExtensions(inner) => Debug::fmt(inner, f),
            ServerHandshake::Certificate(inner) => Debug::fmt(inner, f),
            ServerHandshake::CertificateVerify(inner) => Debug::fmt(inner, f),
            ServerHandshake::Finished(inner) => Debug::fmt(inner, f),
        }
    }
}

impl<N: ArrayLength<u8>> ServerHandshake<N> {
    pub async fn read<T: AsyncRead + AsyncWrite, D: Digest>(
        socket: &mut T,
        len: u16,
        digest: &mut D,
    ) -> Result<Self, TlsError> {
        let mut header = [0; 4];
        let mut pos = 0;
        loop {
            pos += socket.read(&mut header).await?;
            if pos == header.len() {
                break;
            }
        }

        match HandshakeType::of(header[0]) {
            None => Err(TlsError::InvalidHandshake),
            Some(handshake_type) => {
                let length = u32::from_be_bytes([0, header[1], header[2], header[3]]);
                match handshake_type {
                    HandshakeType::ClientHello => Err(TlsError::Unimplemented),
                    HandshakeType::ServerHello => {
                        info!("hash [{:x?}]", &header);
                        digest.update(&header);
                        Ok(ServerHandshake::ServerHello(
                            ServerHello::read(socket, length as usize, digest).await?,
                        ))
                    }
                    HandshakeType::NewSessionTicket => Err(TlsError::Unimplemented),
                    HandshakeType::EndOfEarlyData => Err(TlsError::Unimplemented),
                    HandshakeType::EncryptedExtensions => Err(TlsError::Unimplemented),
                    HandshakeType::Certificate => Err(TlsError::Unimplemented),
                    HandshakeType::CertificateRequest => Err(TlsError::Unimplemented),
                    HandshakeType::CertificateVerify => Err(TlsError::Unimplemented),
                    HandshakeType::Finished => Err(TlsError::Unimplemented),
                    HandshakeType::KeyUpdate => Err(TlsError::Unimplemented),
                    HandshakeType::MessageHash => Err(TlsError::Unimplemented),
                }
            }
        }
    }

    pub fn parse(buf: &mut ParseBuffer) -> Result<Self, TlsError> {
        let handshake_type =
            HandshakeType::of(buf.read_u8().map_err(|_| TlsError::InvalidHandshake)?)
                .ok_or(TlsError::InvalidHandshake)?;

        let content_len = buf.read_u24().map_err(|_| TlsError::InvalidHandshake)?;

        match handshake_type {
            //HandshakeType::ClientHello => {}
            //HandshakeType::ServerHello => {}
            //HandshakeType::NewSessionTicket => {}
            //HandshakeType::EndOfEarlyData => {}
            HandshakeType::EncryptedExtensions => {
                // todo, move digesting up
                Ok(ServerHandshake::EncryptedExtensions(
                    EncryptedExtensions::parse(buf)?,
                ))
            }
            HandshakeType::Certificate => {
                Ok(ServerHandshake::Certificate(Certificate::parse(buf)?))
            }

            //HandshakeType::CertificateRequest => {}
            HandshakeType::CertificateVerify => Ok(ServerHandshake::CertificateVerify(
                CertificateVerify::parse(buf)?,
            )),
            HandshakeType::Finished => Ok(ServerHandshake::Finished(Finished::parse(
                buf,
                content_len,
            )?)),
            //HandshakeType::KeyUpdate => {}
            //HandshakeType::MessageHash => {}
            _ => Err(TlsError::Unimplemented),
        }
    }
}