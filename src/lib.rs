mod incoming;
mod oanda;

use incoming::Incoming;
use worker::{Context, Env, Error, Request, Response, Result, console_error, event};

use crate::oanda::{close_positions, login};

#[event(fetch)]
pub async fn main(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    // Get the incoming body
    let yaml = req.text().await?;
    // parse it
    let incoming = serde_yaml::from_str::<Incoming>(&yaml).map_err(|err| {
        console_error!("parsing incoming message: ({err:?}):\n{yaml}");
        Error::RustError("Internal server error".to_string())
    })?;
    // Authenticate the incoming message
    let Some(incoming) = incoming.authenticate(&env) else {
        return Response::error("Un-authenticated", 401);
    };
    // Log in to oanda
    let Some(oanda_client) = login(&env).await else {
        return Response::error("Un-authenticated", 401);
    };
    // Apply the "action" to the instrument
    match incoming.action() {
        incoming::Action::Close => {
            let instrument = incoming.instrument();
            if close_positions(&oanda_client, &env, instrument).await {
                Response::ok("ok")
            } else {
                Response::error("Unable to close positions for {instrument}", 502)
            }
        }
    }
}

/// Given a secret name, get the value, or error
pub(crate) fn get_secret(name: &str, env: &Env) -> Option<String> {
    env.secret(name)
        .map(|value| value.to_string())
        .inspect_err(|err| console_error!("Error reading secret: {name}: {err:?}"))
        .ok()
}
