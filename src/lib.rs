mod incoming;

use incoming::Incoming;
use worker::{Context, Env, Error, Request, Response, Result, console_error, event};

#[event(fetch)]
pub async fn main(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    // Get the incoming body
    let yaml = req.text().await?;
    // parse it
    let incoming = serde_yaml::from_str::<Incoming>(&yaml).map_err(|err| {
        console_error!("parsing incoming message: ({err:?}):\n{yaml}");
        Error::RustError("Internal server error".to_string())
    })?;
    // Authenticate
    let Some(incoming) = incoming.authenticate(&env) else {
        return Response::error("Un-authenticated", 401);
    };
    Response::ok("ok")
}
