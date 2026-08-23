//! The iAP2 control session (spec §56.3.2) as a `Stream`/`Sink` of messages.
//!
//! [`ControlSession`] sits on a [`Link`] and does two things: it learns the control
//! session's identifier from the Link Synchronization Payload, and it turns session
//! payloads into decoded [`Message`]s in both directions. Everything above it deals in
//! messages, never in sequence numbers or checksums.

use std::{
	collections::VecDeque,
	pin::Pin,
	task::{Context, Poll},
};

use futures::{Sink, Stream, ready};
use log::{debug, warn};

use crate::iap2::{
	error::Error,
	link::{Link, LinkEvent},
	msg::Message,
	packet::Frame,
	sync::LinkParams,
};

/// What the control session reports upwards.
#[derive(Clone, Debug)]
pub enum ControlEvent {
	/// The link handshake finished; the control session is open for business.
	Established(LinkParams),
	/// A decoded control-session message from the accessory.
	Message(Message),
	/// A payload arrived on a session we do not handle.
	OtherSession { session: u8, len: usize },
	/// The link was reset and needs re-establishing.
	Reset,
	/// The accessory is being told we sleep, or told us it does.
	Sleep,
}

/// A control session over a framed iAP2 link.
pub struct ControlSession<T> {
	link: Link<T>,
	session: Option<u8>,
	ready: VecDeque<ControlEvent>,
}

impl<T> ControlSession<T>
where
	T: Stream<Item = Result<Frame, Error>> + Sink<Frame, Error = Error> + Unpin,
{
	pub fn new(link: Link<T>) -> Self {
		Self {
			link,
			session: None,
			ready: VecDeque::new(),
		}
	}

	/// The negotiated control-session identifier, once the handshake has run.
	pub fn session_id(&self) -> Option<u8> {
		self.session
	}

	pub fn established(&self) -> bool {
		self.link.established()
	}

	pub fn params(&self) -> &LinkParams {
		self.link.params()
	}

	/// The link underneath, for sending on a non-control session.
	pub fn link_mut(&mut self) -> &mut Link<T> {
		&mut self.link
	}

	fn absorb(&mut self, event: LinkEvent) {
		match event {
			LinkEvent::Established(params) => {
				self.session = params.control_session();
				self.ready.push_back(ControlEvent::Established(params));
			}
			LinkEvent::Payload { session, data } => {
				if Some(session) != self.session {
					self.ready.push_back(ControlEvent::OtherSession {
						session,
						len: data.len(),
					});
					return;
				}
				match Message::decode_all(&data) {
					Ok(msgs) if msgs.is_empty() => {
						warn!(
							"control session payload held no message: {:02x?}",
							data.as_ref()
						)
					}
					Ok(msgs) => self
						.ready
						.extend(msgs.into_iter().map(ControlEvent::Message)),
					// §56.3.2.2 says to ignore messages we cannot make sense of rather
					// than tear the connection down.
					Err(err) => warn!("ignoring undecodable control-session message: {err}"),
				}
			}
			LinkEvent::Reset => self.ready.push_back(ControlEvent::Reset),
			LinkEvent::Sleep => self.ready.push_back(ControlEvent::Sleep),
			LinkEvent::Detected => debug!("iAP2 detection exchanged"),
			LinkEvent::Iap1Detected => warn!("accessory believes we only speak iAP1"),
		}
	}
}

impl<T> Stream for ControlSession<T>
where
	T: Stream<Item = Result<Frame, Error>> + Sink<Frame, Error = Error> + Unpin,
{
	type Item = Result<ControlEvent, Error>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();
		loop {
			if let Some(event) = this.ready.pop_front() {
				return Poll::Ready(Some(Ok(event)));
			}
			match ready!(Pin::new(&mut this.link).poll_next(cx)) {
				Some(Ok(event)) => this.absorb(event),
				Some(Err(err)) => return Poll::Ready(Some(Err(err))),
				None => return Poll::Ready(None),
			}
		}
	}
}

impl<T> Sink<Message> for ControlSession<T>
where
	T: Stream<Item = Result<Frame, Error>> + Sink<Frame, Error = Error> + Unpin,
{
	type Error = Error;

	fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		Pin::new(&mut self.get_mut().link).poll_ready(cx)
	}

	fn start_send(self: Pin<&mut Self>, msg: Message) -> Result<(), Error> {
		let this = self.get_mut();
		let session = this
			.session
			.ok_or(Error::Timeout("control session negotiation"))?;
		debug!("tx {msg:?}");
		let bytes = msg.encode()?;
		Pin::new(&mut this.link).start_send((session, bytes))
	}

	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		Pin::new(&mut self.get_mut().link).poll_flush(cx)
	}

	fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		Pin::new(&mut self.get_mut().link).poll_close(cx)
	}
}
