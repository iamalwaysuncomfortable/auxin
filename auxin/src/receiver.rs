use auxin_protos::{WebSocketMessage, WebSocketMessage_Type, WebSocketRequestMessage, WebSocketResponseMessage};
use futures::{Sink, SinkExt, Stream, StreamExt};
use log::{debug, info, warn};
use rand::{CryptoRng, RngCore};
use std::{fmt::Debug};
use std::{
	pin::Pin,
};

use crate::message::fix_protobuf_buf;
use crate::{message::{MessageIn, MessageInError, MessageOut}, state::AuxinStateManager};
use crate::net::{AuxinNetManager, AuxinWebsocketConnection};
use crate::AuxinApp; 
use crate::address::AuxinAddress;

// (Try to) read a raw byte buffer as a Signal Envelope protobuf.
pub fn read_envelope_from_bin(buf: &[u8]) -> crate::Result<auxin_protos::Envelope> {
	let new_buf = fix_protobuf_buf(&Vec::from(buf))?;
	let mut reader = protobuf::CodedInputStream::from_bytes(new_buf.as_slice());
	Ok(reader.read_message()?)
}

#[derive(Debug)]
pub enum ReceiveError {
	NetSpecific(String),
	SendErr(String),
	InError(MessageInError),
	StoreStateError(String),
	ReconnectErr(String),
	AttachmentErr(String),
	DeserializeErr(String),
	UnknownWebsocketTy,
}

impl std::fmt::Display for ReceiveError {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		match self {
			Self::NetSpecific(e) => {
				write!(f, "Net manager implementation produced an error: {:?}", e)
			}
			Self::SendErr(e) => write!(
				f,
				"Net manager errored while attempting to send a response: {:?}",
				e
			),
			Self::InError(e) => write!(f, "Unable to decode or decrypt message: {:?}", e),
			Self::StoreStateError(e) => {
				write!(f, "Unable to store state after receiving message: {:?}", e)
			}
			Self::ReconnectErr(e) => {
				write!(f, "Error while attempting to reconnect websocket: {:?}", e)
			}
			Self::AttachmentErr(e) => {
				write!(f, "Error while attempting to retrieve attachment: {:?}", e)
			}
			Self::UnknownWebsocketTy => write!(f, "Websocket message type is Unknown!"),
			Self::DeserializeErr(e) => write!(f, "Failed to deserialize incoming message: {:?}", e),
		}
	}
}

impl std::error::Error for ReceiveError {}

impl From<MessageInError> for ReceiveError {
	fn from(val: MessageInError) -> Self {
		Self::InError(val)
	}
}

type OutstreamT<N> = Pin<
	Box<
		dyn Sink<
			<<N as AuxinNetManager>::W as AuxinWebsocketConnection>::Message,
			Error = <<N as AuxinNetManager>::W as AuxinWebsocketConnection>::SinkError,
		>,
	>,
>;
type InstreamT<N> = Pin<
	Box<
		dyn Stream<
			Item = std::result::Result<
				<<N as AuxinNetManager>::W as AuxinWebsocketConnection>::Message,
				<<N as AuxinNetManager>::W as AuxinWebsocketConnection>::StreamError,
			>,
		>,
	>,
>;

pub struct AuxinReceiver<'a, R, N, S>
where
	R: RngCore + CryptoRng,
	N: AuxinNetManager,
	S: AuxinStateManager,
{
	pub(crate) app: &'a mut AuxinApp<R, N, S>,
	//Disregard these type signatures. They are weird and gnarly for unavoidable reasons.
	pub(crate) outstream: OutstreamT<N>,
	pub(crate) instream: InstreamT<N>,
}

impl<'a, R, N, S> AuxinReceiver<'a, R, N, S>
where
	R: RngCore + CryptoRng,
	N: AuxinNetManager,
	S: AuxinStateManager,
{
	pub async fn new(app: &'a mut AuxinApp<R, N, S>) -> crate::Result<AuxinReceiver<'a, R, N, S>> {
		let ws = app
			.net
			.connect_to_signal_websocket(app.context.identity.clone())
			.await?;
		let (outstream, instream) = ws.into_streams();
		Ok(AuxinReceiver {
			app,
			outstream,
			instream,
		})
	}

	/// Notify the server that we have received a message. If it is a non-receipt Signal message, we will send our receipt indicating we got this message.
	async fn acknowledge_message(
		&mut self,
		msg: &Option<MessageIn>,
		req: &WebSocketRequestMessage,
	) -> std::result::Result<(), ReceiveError> {
		// Sending responses goes here.
		let reply_id = req.get_id();
		let mut res = WebSocketResponseMessage::default();
		res.set_id(reply_id);
		res.set_status(200); // Success
		res.set_message(String::from("OK"));
		res.set_headers(req.get_headers().clone().into());
		let mut res_m = WebSocketMessage::default();
		res_m.set_response(res);
		res_m.set_field_type(WebSocketMessage_Type::RESPONSE);

		self.outstream
			.send(res_m.into())
			.await
			.map_err(|e| ReceiveError::SendErr(format!("{:?}", e)))?;

		if let Some(msg) = msg {
			// Send receipts if we have to.
			if msg.needs_receipt() {
				let receipt = msg.generate_receipt(auxin_protos::ReceiptMessage_Type::DELIVERY);
				self.app
					.send_message(&msg.remote_address.address, receipt)
					.await
					.map_err(|e| ReceiveError::SendErr(format!("{:?}", e)))?;
			}
		}

		self.outstream
			.flush()
			.await
			.map_err(|e| ReceiveError::SendErr(format!("{:?}", e)))?;
		Ok(())
	}
	async fn next_inner(
		&mut self,
		wsmessage: &auxin_protos::WebSocketMessage,
	) -> std::result::Result<Option<MessageIn>, ReceiveError> {
		match wsmessage.get_field_type() {
			auxin_protos::WebSocketMessage_Type::UNKNOWN => Err(ReceiveError::UnknownWebsocketTy),
			auxin_protos::WebSocketMessage_Type::REQUEST => {
				let req = wsmessage.get_request();

				let envelope = read_envelope_from_bin(req.get_body())
					.map_err(|e| ReceiveError::DeserializeErr(format!("{:?}", e)))?;

				let maybe_a_message = self.app.handle_inbound_envelope(envelope).await;
				//.map_err(|e| ReceiveError::InError(e))?;

				// Done this way to ensure invalid messages are still acknowledged, to clear them from the queue.
				let msg = match maybe_a_message {
					Err(MessageInError::ProtocolError(e)) => {
						warn!("Message failed to decrypt - ignoring error and continuing to receive messages to clear out prior bad state. Error was: {:?}", e);
						None
					}
					Err(MessageInError::DecodingProblem(e)) => {
						warn!("Message failed to decode (bad envelope?) - ignoring error and continuing to receive messages to clear out prior bad state. Error was: {:?}", e);
						None
					}
					Err(e) => {
						return Err(e.into());
					}
					Ok(m) => m,
					//It's okay that this can return None, because next() will continue to poll on a None return from this method, and try getting more messages.
					//"None" returns from handle_inbound_envelope() imply messages meant for the protocol rather than the end-user.
				};

				//This will at least acknowledge to WebSocket that we have received this message.
				self.acknowledge_message(&msg, &req).await?;

				if let Some(msg) = &msg {
					//Save session.
					self.app
						.state_manager
						.save_peer_sessions(&msg.remote_address.address, &self.app.context)
						.map_err(|e| ReceiveError::StoreStateError(format!("{:?}", e)))?;
				}
				Ok(msg)
			}
			auxin_protos::WebSocketMessage_Type::RESPONSE => {
				let res = wsmessage.get_response();
				info!("WebSocket response message received: {:?}", res);
				Ok(None)
			}
		}
	}
	/// Polls for the next available message.  Returns none for end of stream.
	pub async fn next(&mut self) -> Option<std::result::Result<MessageIn, ReceiveError>> {
		//Try up to 64 times if necessary.
		for _ in 0..64 {
			let msg = self.instream.next().await;

			match msg {
				None => {
					return None;
				}
				Some(Err(e)) => {
					return Some(Err(ReceiveError::NetSpecific(format!("{:?}", e))));
				}
				Some(Ok(m)) => {
					let wsmessage: WebSocketMessage = m.into();
					//Check to see if we're done.
					if wsmessage.get_field_type() == WebSocketMessage_Type::REQUEST {
						let req = wsmessage.get_request();
						if req.has_path() {
							// The server has sent us all the messages it has waiting for us.
							if req.get_path().contains("/api/v1/queue/empty") {
								debug!("Received an /api/v1/queue/empty message. Message receiving complete.");
								//Acknowledge we received the end-of-queue and do many clunky error-handling things:
								let res = self
									.acknowledge_message(&None, &req)
									.await
									.map_err(|e| ReceiveError::SendErr(format!("{:?}", e)));
								let res = match res {
									Ok(()) => None,
									Err(e) => Some(Err(e)),
								};

								// Receive operation is done. Indicate there are no further messages left to poll for.
								return res; //Usually this returns None.
							}
						}
					}

					//Actually parse our message otherwise.
					match self.next_inner(&wsmessage).await {
						Ok(Some(message)) => return Some(Ok(message)),
						Ok(None) =>
							/*Message failed to decode - ignoring error and continuing to receive messages to clear out prior bad state*/
							{}
						Err(e) => return Some(Err(e)),
					}
				}
			}
		}
		None
	}

	/// Convenience method so we don't have to work around the borrow checker to call send_message on our app when the Receiver has an &mut app.
	pub async fn send_message(
		&mut self,
		recipient_addr: &AuxinAddress,
		message: MessageOut,
	) -> crate::Result<()> {
		self.app.send_message(recipient_addr, message).await
	}

	/// Request additional messages (to continue polling for messages after "/api/v1/queue/empty" has been sent). This is a GET request with path GET /v1/messages/
	pub async fn refresh(&mut self) -> std::result::Result<(), ReceiveError> {
		let mut req = WebSocketRequestMessage::default();
		req.set_id(self.app.rng.next_u64());
		req.set_verb("GET".to_string());
		req.set_path("/v1/messages/".to_string());
		let mut req_m = WebSocketMessage::default();
		req_m.set_request(req);
		req_m.set_field_type(WebSocketMessage_Type::REQUEST);

		self.outstream
			.send(req_m.into())
			.await
			.map_err(|e| ReceiveError::SendErr(format!("{:?}", e)))?;

		self.outstream
			.flush()
			.await
			.map_err(|e| ReceiveError::SendErr(format!("{:?}", e)))?;

		Ok(())
	}

	pub async fn reconnect(&mut self) -> crate::Result<()> {
		self.outstream
			.close()
			.await
			.map_err(|e| ReceiveError::ReconnectErr(format!("Could not close: {:?}", e)))?;
		let ws = self
			.app
			.net
			.connect_to_signal_websocket(self.app.context.identity.clone())
			.await?;
		let (outstream, instream) = ws.into_streams();

		self.outstream = outstream;
		self.instream = instream;

		Ok(())
	}

	pub fn borrow_app(&mut self) -> &mut AuxinApp<R, N, S> {
		self.app
	}
}