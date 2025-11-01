use super::{BcCamera, Error, Result};
use crate::bc::model::*;

impl BcCamera {
    /// Create a handler to respond to keep alive messages
    /// These messages are sent by the camera so we listen to
    /// a message ID rather than setting a message number and
    /// responding to it
    pub async fn keepalive(&self) -> Result<()> {
        let connection = self.get_connection();
        connection
            .handle_msg(MSG_ID_UDP_KEEP_ALIVE, |bc| {
                Box::pin(async move {
                    Some(Bc {
                        meta: BcMeta {
                            msg_id: MSG_ID_UDP_KEEP_ALIVE,
                            channel_id: bc.meta.channel_id,
                            msg_num: bc.meta.msg_num,
                            stream_type: bc.meta.stream_type,
                            response_code: 200,
                            class: 0x6414,
                        },
                        body: BcBody::ModernMsg(ModernMsg {
                            ..Default::default()
                        }),
                    })
                })
            })
            .await?;
        Ok(())
    }

    /// Send an active UDP keepalive message to the camera
    /// This is used to keep NAT holes open and prevent connection timeouts
    /// on UDP connections. Returns Ok(()) on success.
    pub async fn send_keepalive(&self) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub_keepalive = connection.subscribe(MSG_ID_UDP_KEEP_ALIVE, msg_num).await?;

        let keepalive_msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_UDP_KEEP_ALIVE,
                channel_id: self.channel_id,
                msg_num,
                response_code: 0,
                stream_type: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                ..Default::default()
            }),
        };

        sub_keepalive.send(keepalive_msg).await?;

        // Wait for acknowledgment with a short timeout
        // Some cameras may not respond, so we don't treat this as fatal
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sub_keepalive.recv()
        ).await {
            Ok(Ok(msg)) => {
                if msg.meta.response_code != 200 {
                    log::trace!("Keepalive response code: {}", msg.meta.response_code);
                    return Err(Error::CameraServiceUnavailable {
                        id: msg.meta.msg_id,
                        code: msg.meta.response_code,
                    });
                }
                Ok(())
            }
            Ok(Err(e)) => {
                log::trace!("Keepalive receive error: {:?}", e);
                Err(e)
            }
            Err(_) => {
                // Timeout is not fatal - some cameras don't respond to keepalives
                log::trace!("Keepalive timeout (camera may not require acknowledgment)");
                Ok(())
            }
        }
    }
}
