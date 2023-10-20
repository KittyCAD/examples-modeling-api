use color_eyre::{
    eyre::{bail, Context, Error},
    Result,
};
use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use kittycad::types::{
    FailureWebSocketResponse, ModelingCmd, OkModelingCmdResponse, OkWebSocketResponseData,
    PathSegment, Point3D, SuccessWebSocketResponse, WebSocketRequest,
};
use reqwest::Upgraded;
use std::{env, io::Cursor, time::Duration};
use tokio::time::timeout;
use tokio_tungstenite::{tungstenite::Message as WsMsg, WebSocketStream};
use uuid::Uuid;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Set up input and output.
    let token = env::var("KITTYCAD_API_TOKEN").context("You must set $KITTYCAD_API_TOKEN")?;
    let img_output_path = env::var("IMAGE_OUTPUT_PATH").unwrap_or_else(|_| "model.png".to_owned());
    let client = kittycad::Client::new(token);

    // Connect to KittyCAD modeling API via WebSocket.
    let ws = client
        .modeling()
        .commands_ws(Some(30), Some(false), Some(480), Some(640), Some(false))
        .await
        .context("Could not open WebSocket to KittyCAD Modeling API")?;

    // Prepare to write to/read from the WebSocket.
    let (write, read) = tokio_tungstenite::WebSocketStream::from_raw_socket(
        ws,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await
    .split();

    draw_cube(write, 10.0).await?;
    export_png(read, img_output_path).await?;
    Ok(())
}

async fn draw_cube(
    mut write_to_ws: SplitSink<WebSocketStream<Upgraded>, WsMsg>,
    width: f64,
) -> Result<()> {
    // All messages to the KittyCAD Modeling API will be sent over the WebSocket as Text.
    // The text will contain JSON representing a `ModelingCmdReq`.
    let to_msg = |cmd, cmd_id| {
        WsMsg::Text(
            serde_json::to_string(&WebSocketRequest::ModelingCmdReq { cmd, cmd_id }).unwrap(),
        )
    };

    // Now the WebSocket is set up and ready to use!
    // We can start sending commands.

    // Start a path
    let path_id = Uuid::new_v4();
    write_to_ws
        .send(to_msg(ModelingCmd::StartPath {}, path_id))
        .await?;

    // Draw the path in a square shape.
    let start = Point3D {
        x: -width,
        y: -width,
        z: -width,
    };

    write_to_ws
        .send(to_msg(
            ModelingCmd::MovePathPen {
                path: path_id,
                to: start.clone(),
            },
            Uuid::new_v4(),
        ))
        .await?;

    let points = [
        Point3D {
            x: width,
            y: -width,
            z: -width,
        },
        Point3D {
            x: width,
            y: width,
            z: -width,
        },
        Point3D {
            x: -width,
            y: width,
            z: -width,
        },
        start,
    ];
    for point in points {
        write_to_ws
            .send(to_msg(
                ModelingCmd::ExtendPath {
                    path: path_id,
                    segment: PathSegment::Line {
                        end: point,
                        relative: false,
                    },
                },
                Uuid::new_v4(),
            ))
            .await?;
    }

    // Extrude the square into a cube.
    write_to_ws
        .send(to_msg(ModelingCmd::ClosePath { path_id }, Uuid::new_v4()))
        .await?;
    write_to_ws
        .send(to_msg(
            ModelingCmd::Extrude {
                cap: true,
                distance: width * 2.0,
                target: path_id,
            },
            Uuid::new_v4(),
        ))
        .await?;
    write_to_ws
        .send(to_msg(
            ModelingCmd::TakeSnapshot {
                format: kittycad::types::ImageFormat::Png,
            },
            Uuid::new_v4(),
        ))
        .await?;

    // Finish sending
    drop(write_to_ws);
    Ok(())
}

async fn export_png(
    mut read_from_ws: SplitStream<WebSocketStream<Upgraded>>,
    img_output_path: String,
) -> Result<()> {
    fn ws_resp_from_text(text: &str) -> Result<OkWebSocketResponseData> {
        let resp: WebSocketResponse = serde_json::from_str(text)?;
        match resp {
            WebSocketResponse::Success(s) => {
                assert!(s.success);
                Ok(s.resp)
            }
            WebSocketResponse::Failure(mut f) => {
                assert!(!f.success);
                let Some(err) = f.errors.pop() else {
                    bail!("websocket failure, no error given");
                };
                bail!("websocket failure: {err}");
            }
        }
    }

    fn text_from_ws(msg: WsMsg) -> Result<Option<String>> {
        match msg {
            // We expect all responses to be text.
            WsMsg::Text(text) => Ok(Some(text)),
            // WebSockets might sometimes send Pongs, that's OK. It's just for healthchecks or to
            // keep the WebSocket open. We can ignore them.
            WsMsg::Pong(_) => Ok(None),
            other => bail!("only expected text or pong responses, but received {other:?}"),
        }
    }

    // Get Websocket messages from API server
    let server_responses = async move {
        while let Some(msg) = read_from_ws.next().await {
            let Some(resp) = text_from_ws(msg?)? else {
                continue;
            };
            let resp = ws_resp_from_text(&resp)?;
            if let OkWebSocketResponseData::Modeling { modeling_response } = resp {
                match modeling_response {
                    OkModelingCmdResponse::Empty {} => {}
                    OkModelingCmdResponse::TakeSnapshot { data } => {
                        let mut img = image::io::Reader::new(Cursor::new(data.contents));
                        img.set_format(image::ImageFormat::Png);
                        let img = img.decode()?;
                        img.save(img_output_path)?;
                        break;
                    }
                    _ => {}
                }
            }
        }
        Ok::<_, Error>(())
    };
    timeout(Duration::from_secs(10), server_responses).await??;
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WebSocketResponse {
    Success(SuccessWebSocketResponse),
    Failure(FailureWebSocketResponse),
}