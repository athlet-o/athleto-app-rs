//! Maud layouts and the storefront pages (product grid + product detail),
//! plus shared page/fragment helpers used by the auth and cart modules.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::auth::{AuthUser, MaybeUser};
use crate::db::{self, Product};
use crate::SharedState;

pub const APP_CSS: &str = r###"
:root {
  color-scheme: light;
  --ink: #12323a;
  --muted: #516872;
  --paper: #f8fbff;
  --paper-2: #ffffff;
  --line: rgba(18, 50, 58, 0.16);
  --green: #53d86a;
  --green-dark: #168943;
  --aqua: #27c9c3;
  --blue: #355dff;
  --coral: #ff6f61;
  --yellow: #ffd84d;
  --berry: #d9498b;
  --shadow: 0 22px 55px rgba(18, 50, 58, 0.16);
}

* { box-sizing: border-box; }

html { scroll-behavior: smooth; }

body {
  margin: 0;
  min-width: 320px;
  min-height: 100vh;
  display: flex;
  flex-direction: column;
  background: var(--paper);
  color: var(--ink);
  font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}

a { color: inherit; }

main { flex: 1; }

.wordmark { font-weight: 950; }

.wordmark .o { position: relative; font-size: 1.16em; }

.wordmark .o::after {
  content: "";
  position: absolute;
  left: 4%;
  right: 4%;
  bottom: -0.06em;
  height: 0.1em;
  border-radius: 999px;
  background: var(--green-dark);
}

.site-header {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 14px;
  padding: 16px clamp(18px, 4%, 56px);
  border-bottom: 2px solid var(--ink);
  background: var(--paper-2);
}

.brand-lockup {
  display: inline-flex;
  align-items: center;
  gap: 12px;
  color: var(--ink);
  text-decoration: none;
}

.brand-mark {
  display: inline-grid;
  width: 44px;
  height: 44px;
  place-items: center;
  border: 3px solid var(--ink);
  border-radius: 13px;
  background: var(--yellow);
  color: var(--ink);
  font-weight: 950;
  font-size: 1.15rem;
  box-shadow: 5px 5px 0 var(--ink);
}

.brand-name {
  font-weight: 950;
  font-size: 1.4rem;
  letter-spacing: 0;
}

.site-nav {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
}

.site-nav a,
.site-nav button {
  display: inline-flex;
  min-height: 38px;
  align-items: center;
  padding: 6px 16px;
  border: 2px solid var(--ink);
  border-radius: 999px;
  background: var(--paper-2);
  color: var(--ink);
  font: inherit;
  font-weight: 900;
  text-decoration: none;
  cursor: pointer;
  box-shadow: 3px 3px 0 var(--ink);
  transition: transform 120ms ease, box-shadow 120ms ease;
}

.site-nav a:hover,
.site-nav button:hover {
  transform: translate(2px, 2px);
  box-shadow: 1px 1px 0 var(--ink);
}

.site-nav a.accent { background: var(--green); }

.nav-user {
  color: var(--muted);
  font-weight: 700;
  font-size: 0.92rem;
}

.hero {
  padding: 56px clamp(18px, 4%, 56px) 40px;
  background: var(--paper);
  border-bottom: 2px solid var(--ink);
  position: relative;
  overflow: hidden;
}

.hero::before {
  content: "";
  position: absolute;
  inset: auto 0 0 0;
  height: 26px;
  background:
    repeating-linear-gradient(
      90deg,
      rgba(255, 216, 77, 0.52) 0 56px,
      rgba(83, 216, 106, 0.34) 56px 112px,
      rgba(39, 201, 195, 0.32) 112px 168px,
      rgba(255, 111, 97, 0.34) 168px 224px
    );
  opacity: 0.72;
}

.hero > * { position: relative; z-index: 1; }

.eyebrow {
  width: fit-content;
  margin: 0 0 16px;
  padding: 8px 14px;
  border: 2px solid var(--ink);
  border-radius: 999px;
  background: var(--paper-2);
  color: var(--green-dark);
  font-weight: 900;
  font-size: 0.82rem;
  text-transform: uppercase;
  letter-spacing: 0;
}

h1, h2, h3, p { margin-top: 0; }

.hero h1 {
  max-width: 720px;
  margin-bottom: 14px;
  font-size: clamp(2.4rem, 6vw, 4.2rem);
  line-height: 0.98;
  font-weight: 950;
}

.hero h1 em {
  font-style: normal;
  color: var(--green-dark);
}

.lede {
  max-width: 640px;
  margin-bottom: 0;
  color: var(--muted);
  font-size: 1.15rem;
  line-height: 1.6;
}

.section {
  padding: 44px clamp(18px, 4%, 56px);
}

.section > h2 {
  margin-bottom: 20px;
  font-size: 1.8rem;
  font-weight: 950;
}

.product-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
  gap: 20px;
}

.product-card {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 20px;
  border: 2px solid var(--ink);
  border-radius: 8px;
  background: var(--paper-2);
  box-shadow: 6px 6px 0 var(--ink), 0 26px 44px -18px rgba(18, 50, 58, 0.35);
}

.product-card.detail { max-width: 640px; }

.card-top {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 10px;
}

.product-card h3 {
  margin: 0;
  font-size: 1.55rem;
  font-weight: 950;
  line-height: 1;
}

.product-card h3 a { text-decoration: none; }
.product-card h3 a:hover { color: var(--green-dark); }

.subname {
  margin-top: 6px;
  color: var(--green-dark);
  font-weight: 900;
  font-size: 0.85rem;
  text-transform: uppercase;
  letter-spacing: 0.08em;
}

.card-chips {
  display: flex;
  flex-direction: column;
  align-items: flex-end;
  gap: 8px;
  flex-shrink: 0;
}

.cal-chip {
  display: inline-flex;
  align-items: center;
  padding: 6px 11px;
  border: 2px solid var(--ink);
  border-radius: 999px;
  background: var(--paper-2);
  color: var(--ink);
  font-size: 0.85rem;
  font-weight: 900;
  box-shadow: 0 6px 14px rgba(18, 50, 58, 0.15);
  white-space: nowrap;
}

.format-badge {
  flex-shrink: 0;
  padding: 5px 12px;
  border: 2px solid var(--ink);
  border-radius: 999px;
  font-weight: 900;
  font-size: 0.78rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--ink);
  white-space: nowrap;
}

.format-badge.cup { background: var(--aqua); }
.format-badge.powder { background: var(--coral); color: #ffffff; }

.product-desc {
  margin: 0;
  color: var(--muted);
  line-height: 1.55;
  font-size: 0.98rem;
}

.stat-row {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}

.stat-row span {
  display: inline-flex;
  align-items: center;
  min-height: 34px;
  padding: 7px 10px;
  border: 1px solid rgba(18, 50, 58, 0.2);
  border-radius: 999px;
  background: var(--paper);
  color: var(--ink);
  font-weight: 800;
  font-size: 0.85rem;
  line-height: 1.1;
}

.stat-row span.stat-protein { color: var(--green-dark); font-weight: 900; }

.card-buy {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  margin-top: auto;
}

.price {
  font-size: 1.35rem;
  font-weight: 950;
  color: var(--ink);
}

button.buy,
a.button,
button.primary {
  display: inline-flex;
  min-height: 44px;
  align-items: center;
  justify-content: center;
  padding: 10px 18px;
  border: 2px solid var(--ink);
  border-radius: 999px;
  background: var(--green);
  color: var(--ink);
  font: inherit;
  font-weight: 900;
  text-decoration: none;
  cursor: pointer;
  box-shadow: 4px 4px 0 var(--ink);
  transition: transform 120ms ease, box-shadow 120ms ease;
}

button.buy:hover,
a.button:hover,
button.primary:hover {
  transform: translate(2px, 2px);
  box-shadow: 2px 2px 0 var(--ink);
}

a.button.ghost { background: var(--paper-2); }

button.danger { background: var(--coral); color: #ffffff; }

.card-status { min-height: 1.4em; font-weight: 800; font-size: 0.92rem; }
.card-status .added { color: var(--green-dark); }
.card-status a { color: var(--blue); }

.notice {
  max-width: 640px;
  padding: 16px 18px;
  border: 2px solid var(--ink);
  border-left: 10px solid var(--yellow);
  border-radius: 8px;
  background: var(--paper-2);
  box-shadow: 4px 4px 0 var(--ink), 0 18px 34px -18px rgba(18, 50, 58, 0.35);
  color: var(--ink);
  line-height: 1.55;
}

.notice.error { border-left-color: var(--coral); }
.notice.success { border-left-color: var(--green); }

.auth-card {
  max-width: 440px;
  display: flex;
  flex-direction: column;
  gap: 14px;
  padding: 24px;
  border: 2px solid var(--ink);
  border-radius: 8px;
  background: var(--paper-2);
  box-shadow: 6px 6px 0 var(--ink), 0 26px 44px -18px rgba(18, 50, 58, 0.35);
}

.auth-card label {
  display: flex;
  flex-direction: column;
  gap: 6px;
  font-weight: 800;
  font-size: 0.92rem;
}

.auth-card input {
  min-height: 42px;
  padding: 8px 12px;
  border: 2px solid rgba(18, 50, 58, 0.3);
  border-radius: 10px;
  background: var(--paper);
  color: var(--ink);
  font: inherit;
}

.auth-card input:focus {
  outline: none;
  border-color: var(--green-dark);
}

.auth-alt { color: var(--muted); font-size: 0.92rem; }
.auth-alt a { color: var(--green-dark); font-weight: 800; }

.cart-table {
  width: 100%;
  border-collapse: collapse;
  border: 2px solid var(--ink);
  background: var(--paper-2);
  box-shadow: 6px 6px 0 var(--ink), 0 26px 44px -18px rgba(18, 50, 58, 0.35);
  border-radius: 8px;
  overflow: hidden;
}

.cart-table th,
.cart-table td {
  padding: 12px 16px;
  text-align: left;
  border-bottom: 2px solid var(--line);
}

.cart-table th {
  background: var(--paper);
  font-size: 0.82rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--muted);
}

.cart-table tr:last-child td { border-bottom: none; }

.cart-product .subname { margin-top: 2px; }

.cart-cal {
  color: var(--muted);
  font-weight: 800;
  white-space: nowrap;
}

.cart-total {
  margin-top: 18px;
  font-size: 1.25rem;
  font-weight: 950;
}

.cart-total strong { color: var(--green-dark); }

.cart-actions { margin-top: 18px; display: flex; gap: 12px; flex-wrap: wrap; }

.site-footer {
  padding: 26px clamp(18px, 4%, 56px);
  border-top: 2px solid var(--ink);
  background: var(--paper);
  color: var(--muted);
  font-weight: 700;
  display: flex;
  flex-wrap: wrap;
  gap: 10px;
  justify-content: space-between;
  line-height: 1.55;
}

.site-footer .tagline { color: var(--green-dark); font-weight: 900; }

.auth-section { display: flex; justify-content: center; }
.auth-section .auth-card { width: min(480px, 100%); }
.auth-lede { color: var(--muted); line-height: 1.55; }

.biz-chip {
  display: inline-flex;
  align-items: center;
  margin-left: 8px;
  padding: 3px 10px;
  border: 2px solid var(--ink);
  border-radius: 999px;
  background: var(--aqua);
  font-size: 0.68rem;
  font-weight: 950;
  letter-spacing: 0.08em;
}

.past-logins { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
.chips-label { width: 100%; color: var(--muted); font-weight: 800; font-size: 0.88rem; }
button.chip {
  padding: 6px 14px;
  border: 2px solid var(--ink);
  border-radius: 999px;
  background: var(--yellow);
  color: var(--ink);
  font: inherit;
  font-weight: 800;
  font-size: 0.9rem;
  cursor: pointer;
  box-shadow: 3px 3px 0 var(--ink);
}
button.chip:hover { transform: translate(2px, 2px); box-shadow: 1px 1px 0 var(--ink); }

.code-input {
  font-size: 1.4rem;
  letter-spacing: 0.35em;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}

.qr-box {
  display: grid;
  place-items: center;
  padding: 14px;
  border: 2px solid var(--ink);
  border-radius: 8px;
  background: #ffffff;
  box-shadow: 4px 4px 0 var(--ink);
}
.qr-box svg, .qr-box img { width: 220px; height: 220px; }

.radio-row { flex-direction: row !important; align-items: flex-start; gap: 10px !important; font-weight: 700 !important; }
.radio-row input { min-height: 0 !important; margin-top: 4px; }
.muted-inline { color: var(--muted); font-weight: 700; }
.ok-inline { color: var(--green-dark); font-weight: 900; }

.factor-list { margin: 0 0 14px; padding-left: 18px; line-height: 1.9; }
.inline-form { display: inline; margin-left: 6px; }
button.linklike {
  border: none;
  background: none;
  padding: 0;
  font: inherit;
  font-weight: 800;
  color: var(--blue);
  cursor: pointer;
  text-decoration: underline;
  box-shadow: none;
}
button.linklike.danger-link { color: var(--coral); }

.account-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
  gap: 20px;
  align-items: start;
}
.account-card {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 20px;
  border: 2px solid var(--ink);
  border-radius: 8px;
  background: var(--paper-2);
  box-shadow: 6px 6px 0 var(--ink), 0 26px 44px -18px rgba(18, 50, 58, 0.35);
}
.account-card form { display: flex; flex-direction: column; gap: 10px; }
.account-card label { display: flex; flex-direction: column; gap: 6px; font-weight: 800; font-size: 0.92rem; }
.account-card input, .account-card select {
  min-height: 40px;
  padding: 8px 12px;
  border: 2px solid rgba(18, 50, 58, 0.3);
  border-radius: 10px;
  background: var(--paper);
  color: var(--ink);
  font: inherit;
}

.key-reveal {
  margin-top: 8px;
  padding: 12px;
  border: 2px dashed var(--ink);
  border-radius: 8px;
  background: var(--paper);
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  word-break: break-all;
}

.order-card {
  margin-bottom: 18px;
  padding: 18px 20px;
  border: 2px solid var(--ink);
  border-radius: 8px;
  background: var(--paper-2);
  box-shadow: 5px 5px 0 var(--ink);
}
.order-head { display: flex; flex-wrap: wrap; align-items: center; gap: 12px; margin-bottom: 6px; }

.checkout-form {
  margin-top: 22px;
  display: flex;
  flex-direction: column;
  gap: 12px;
  max-width: 440px;
  padding: 20px;
  border: 2px solid var(--ink);
  border-radius: 8px;
  background: var(--paper-2);
  box-shadow: 5px 5px 0 var(--ink);
}
.checkout-form label { display: flex; flex-direction: column; gap: 6px; font-weight: 800; font-size: 0.92rem; }
.checkout-form select, .checkout-form input {
  min-height: 42px;
  padding: 8px 12px;
  border: 2px solid rgba(18, 50, 58, 0.3);
  border-radius: 10px;
  background: var(--paper);
  font: inherit;
}

.hold-banner {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  align-items: center;
  max-width: 640px;
  margin-bottom: 16px;
  padding: 12px 16px;
  border: 2px solid var(--ink);
  border-left: 10px solid var(--aqua);
  border-radius: 8px;
  background: var(--paper-2);
  font-weight: 800;
  box-shadow: 4px 4px 0 var(--ink);
}
.hold-banner.expired { border-left-color: var(--coral); }
.hold-banner strong { color: var(--green-dark); }
.qty-input { width: 90px; min-height: 38px; padding: 6px 10px; border: 2px solid rgba(18,50,58,0.3); border-radius: 10px; font: inherit; }
"###;

pub fn format_price(cents: i64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

fn wordmark() -> Markup {
    html! {
        span .wordmark { "Athlet" span .o { "O" } }
    }
}

fn product_display_name(product: &Product) -> String {
    product
        .subname
        .as_deref()
        .map(|subname| format!("AthletO {subname}"))
        .unwrap_or_else(|| product.name.clone())
}

/// Shared document shell: dark athletic theme, htmx, header nav, footer.
pub fn layout(title: &str, user: Option<&AuthUser>, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover";
                meta name="theme-color" content="#f8fbff";
                title { (title) }
                style { (PreEscaped(APP_CSS)) }
                script defer="defer" src="https://unpkg.com/htmx.org@2.0.4" {}
            }
            body {
                header .site-header {
                    a .brand-lockup href="/" {
                        span .brand-mark { "AO" }
                        span .brand-name { (wordmark()) }
                    }
                    nav .site-nav {
                        a href="/" { "Shop" }
                        a href="/cart" { "Cart" }
                        @match user {
                            Some(user) => {
                                span .nav-user { (user.email.as_deref().unwrap_or("signed in")) }
                                form method="post" action="/logout" {
                                    button type="submit" { "Log out" }
                                }
                            }
                            None => {
                                a href="/login" { "Log in" }
                                a .accent href="/signup" { "Sign up" }
                            }
                        }
                    }
                }
                main { (content) }
                footer .site-footer {
                    span .tagline { "Wobble hard. Recover clean." }
                    span { (wordmark()) " performance gelatin protein" }
                }
            }
        }
    }
}

/// Fragment shown wherever a feature needs configuration that is missing.
pub fn not_configured_notice(what: &str) -> Markup {
    html! {
        div .notice {
            strong { (what) " is not configured on this deployment. " }
            "The storefront still works; set the missing environment variables to enable this feature."
        }
    }
}

pub fn error_page(message: &str) -> Markup {
    layout(
        "Something wobbled wrong | AthletO",
        None,
        html! {
            section .section {
                h2 { "Something wobbled wrong" }
                div .notice .error { (message) }
                p { a .button .ghost href="/" { "Back to the shop" } }
            }
        },
    )
}

fn product_card(product: &Product) -> Markup {
    let format_class = match product.format {
        db::ProductFormat::Cup => "cup",
        db::ProductFormat::Powder => "powder",
    };
    html! {
        article .product-card {
            div .card-top {
                div {
                    h3 { a href=(format!("/product/{}", product.slug)) { (wordmark()) } }
                    @if let Some(subname) = product.subname.as_deref() {
                        div .subname { (subname) }
                    }
                }
                div .card-chips {
                    span .cal-chip { (product.calories) " cal" }
                    span .format-badge.(format_class) { (product.format.label()) }
                }
            }
            p .product-desc { (product.description) }
            div .stat-row {
                span .stat-protein { (product.protein_g) "g protein" }
                @if product.format == db::ProductFormat::Powder { span { "just add water" } }
            }
            div .card-buy {
                span .price { (format_price(product.price_cents.into())) }
                form hx-post="/cart/items" hx-target="find .card-status" hx-swap="innerHTML" action="/cart/items" method="post" {
                    input type="hidden" name="product_id" value=(product.id);
                    input type="hidden" name="qty" value="1";
                    button .buy type="submit" { "Add to cart" }
                    div .card-status {}
                }
            }
        }
    }
}

/// GET / -- storefront product grid.
pub async fn home(State(state): State<SharedState>, user: MaybeUser) -> Markup {
    let products = load_catalog(&state).await;
    layout(
        "AthletO | performance gelatin protein",
        user.as_ref(),
        html! {
            section .hero {
                p .eyebrow { "Performance gelatin protein" }
                h1 { "Wobble hard. " em { "Recover clean." } }
                p .lede {
                    "Gelatin protein cups and powder packets built for training bags, "
                    "bus rides, and post-lift cooldowns. Protein, fiber, vitamin C, and "
                    "electrolytes -- no sugar rush."
                }
            }
            section .section {
                h2 { "The lineup" }
                div .product-grid {
                    @for product in &products {
                        (product_card(product))
                    }
                }
            }
        },
    )
}

/// GET /product/{slug} -- product detail page.
pub async fn product_page(
    State(state): State<SharedState>,
    user: MaybeUser,
    Path(slug): Path<String>,
) -> Response {
    let product = match &state.pool {
        Some(pool) => match db::product_by_slug(pool, &slug).await {
            Ok(found) => found,
            Err(err) => {
                tracing::warn!(error = %err, "product lookup failed; using built-in catalog");
                fallback_by_slug(&slug)
            }
        },
        None => fallback_by_slug(&slug),
    };

    let Some(product) = product else {
        return (
            StatusCode::NOT_FOUND,
            layout(
                "Not found | AthletO",
                user.as_ref(),
                html! {
                    section .section {
                        h2 { "No such wobble" }
                        div .notice .error { "We could not find that product." }
                        p { a .button .ghost href="/" { "Back to the shop" } }
                    }
                },
            ),
        )
            .into_response();
    };

    let sibling_slug = match product.format {
        db::ProductFormat::Cup => product.slug.replace("-cup", "-powder"),
        db::ProductFormat::Powder => product.slug.replace("-powder", "-cup"),
    };
    let sibling_label = match product.format {
        db::ProductFormat::Cup => "Also available as a powder packet",
        db::ProductFormat::Powder => "Also available as a ready cup",
    };

    layout(
        &format!(
            "{} ({}) | AthletO",
            product_display_name(&product),
            product.format.label()
        ),
        user.as_ref(),
        html! {
            section .section {
                (product_card(&product))
                div .cart-actions {
                    a .button .ghost href=(format!("/product/{sibling_slug}")) { (sibling_label) }
                    a .button .ghost href="/" { "Back to the shop" }
                }
            }
        },
    )
    .into_response()
}

async fn load_catalog(state: &SharedState) -> Vec<Product> {
    match &state.pool {
        Some(pool) => match db::list_products(pool).await {
            Ok(products) if !products.is_empty() => products,
            Ok(_) => {
                tracing::warn!("products table empty; using built-in catalog");
                db::fallback_products()
            }
            Err(err) => {
                tracing::warn!(error = %err, "product query failed; using built-in catalog");
                db::fallback_products()
            }
        },
        None => db::fallback_products(),
    }
}

fn fallback_by_slug(slug: &str) -> Option<Product> {
    db::fallback_products()
        .into_iter()
        .find(|product| product.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_card_preserves_brand_subname_and_format_details() {
        let product = db::fallback_products().remove(0);
        let rendered = product_card(&product).into_string();

        assert!(rendered.contains("Athlet"));
        assert!(rendered.contains("starter"));
        assert!(rendered.contains("90 cal"));
        assert!(rendered.contains("20g protein"));
        assert!(rendered.contains("Add to cart"));
    }
}
