//! Authenticated websocket endpoint pushing HTML fragments for the htmx ws
//! extension. Messages are id-targeted out-of-band swaps (`hx-swap-oob`), so
//! the browser-side wiring is just `hx-ext="ws" ws-connect="/ws"` around the
//! fragment -- no client-side rendering.
//!
//! Today this drives the cart hold countdown: cart mutations broadcast the
//! affected cart id over `AppState::cart_events`, and every connection whose
//! cart matches (plus a 30s re-sync tick) pushes a fresh `#hold-banner`
//! fragment. The 25-55s polling of GET /cart/hold stays in place as the
//! fallback for anonymous carts and browsers without a working socket.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::auth::{AuthUser, MaybeUser};
use crate::cart::hold_banner_div;
use crate::db::{self, CartOwner};
use crate::SharedState;

/// GET /ws -- upgrade for signed-in users only; the session cookie is
/// resolved exactly like every page request (MaybeUser), so an anonymous or
/// expired-session upgrade is rejected before the handshake completes.
pub async fn upgrade(
    State(state): State<SharedState>,
    user: MaybeUser,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(user) = user.0 else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    ws.on_upgrade(move |socket| connection(state, user, socket))
}

async fn connection(state: SharedState, user: AuthUser, mut socket: WebSocket) {
    let owner = CartOwner::User(user.id);
    let mut events = state.cart_events.subscribe();
    // First tick fires immediately (initial sync), then every 30s to catch
    // expiry flips and countdown drift even when nothing mutates the cart.
    let mut ticker = tokio::time::interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if !push_hold_fragment(&state, &owner, &mut socket).await {
                    break;
                }
            }
            event = events.recv() => match event {
                Ok(cart_id) => {
                    if is_own_cart(&state, &owner, cart_id).await
                        && !push_hold_fragment(&state, &owner, &mut socket).await
                    {
                        break;
                    }
                }
                // Missed some events under load: the next tick re-syncs.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            message = socket.recv() => match message {
                // The htmx ws extension may send data; nothing is expected.
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => break,
            },
        }
    }
}

async fn is_own_cart(state: &SharedState, owner: &CartOwner, cart_id: Uuid) -> bool {
    let Some(conn) = &state.pool else {
        return false;
    };
    matches!(db::find_cart(conn, owner).await, Ok(Some(own)) if own == cart_id)
}

/// Send the current `#hold-banner` state as an OOB fragment; returns false
/// once the socket is gone. Browsers without the banner in the DOM simply
/// ignore the swap.
async fn push_hold_fragment(state: &SharedState, owner: &CartOwner, socket: &mut WebSocket) -> bool {
    let Some(conn) = &state.pool else {
        return true; // degraded mode: keep the socket open, nothing to push
    };
    let seconds_left = match db::find_cart(conn, owner).await {
        Ok(Some(cart_id)) => match db::cart_hold_until(conn, cart_id).await {
            Ok(Some(until)) => (until - chrono::Utc::now()).num_seconds().max(0),
            Ok(None) => 0,
            Err(err) => {
                tracing::warn!(error = %err, "hold lookup for ws push failed");
                return true;
            }
        },
        Ok(None) => 0,
        Err(err) => {
            tracing::warn!(error = %err, "cart lookup for ws push failed");
            return true;
        }
    };
    let fragment = hold_banner_div(seconds_left, true).into_string();
    socket.send(Message::Text(fragment.into())).await.is_ok()
}
