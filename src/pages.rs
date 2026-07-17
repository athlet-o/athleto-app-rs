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
  color-scheme: dark;
  --bg: #0b1519;
  --bg-2: #10222a;
  --card: #132831;
  --ink: #eef7f4;
  --muted: #93abb0;
  --line: rgba(238, 247, 244, 0.16);
  --edge: #061013;
  --green: #53d86a;
  --green-dark: #168943;
  --aqua: #27c9c3;
  --blue: #355dff;
  --coral: #ff6f61;
  --yellow: #ffd84d;
  --berry: #d9498b;
  --shadow: 0 22px 55px rgba(0, 0, 0, 0.45);
}

* { box-sizing: border-box; }

html { scroll-behavior: smooth; }

body {
  margin: 0;
  min-width: 320px;
  min-height: 100vh;
  display: flex;
  flex-direction: column;
  background: var(--bg);
  color: var(--ink);
  font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}

a { color: inherit; }

main { flex: 1; }

.site-header {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 14px;
  padding: 16px clamp(18px, 4%, 56px);
  border-bottom: 2px solid var(--line);
  background: var(--bg-2);
}

.brand-lockup {
  display: inline-flex;
  align-items: center;
  gap: 12px;
  text-decoration: none;
}

.brand-mark {
  display: inline-grid;
  width: 44px;
  height: 44px;
  place-items: center;
  border: 3px solid var(--edge);
  border-radius: 13px;
  background: var(--yellow);
  color: var(--edge);
  font-weight: 950;
  font-size: 1.15rem;
  box-shadow: 5px 5px 0 var(--edge);
}

.brand-name {
  font-weight: 950;
  font-size: 1.4rem;
  letter-spacing: 0.01em;
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
  border: 2px solid var(--edge);
  border-radius: 999px;
  background: var(--card);
  color: var(--ink);
  font: inherit;
  font-weight: 800;
  text-decoration: none;
  cursor: pointer;
  box-shadow: 3px 3px 0 var(--edge);
  transition: transform 120ms ease, box-shadow 120ms ease;
}

.site-nav a:hover,
.site-nav button:hover {
  transform: translate(2px, 2px);
  box-shadow: 1px 1px 0 var(--edge);
}

.site-nav a.accent { background: var(--green); color: var(--edge); }

.nav-user {
  color: var(--muted);
  font-weight: 700;
  font-size: 0.92rem;
}

.hero {
  padding: 56px clamp(18px, 4%, 56px) 40px;
  background:
    radial-gradient(120% 130% at 85% -20%, rgba(39, 201, 195, 0.18) 0%, transparent 55%),
    radial-gradient(110% 120% at 5% 110%, rgba(83, 216, 106, 0.14) 0%, transparent 55%),
    var(--bg);
  border-bottom: 2px solid var(--line);
}

.eyebrow {
  width: fit-content;
  margin: 0 0 16px;
  padding: 7px 14px;
  border: 2px solid var(--edge);
  border-radius: 999px;
  background: var(--yellow);
  color: var(--edge);
  font-weight: 900;
  font-size: 0.82rem;
  text-transform: uppercase;
  letter-spacing: 0.06em;
  box-shadow: 3px 3px 0 var(--edge);
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
  color: var(--green);
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
  border: 2px solid var(--edge);
  border-radius: 18px;
  background: var(--card);
  box-shadow: 7px 7px 0 var(--edge);
}

.product-card.detail { max-width: 640px; }

.card-top {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
}

.product-card h3 {
  margin: 0;
  font-size: 1.3rem;
  font-weight: 950;
}

.product-card h3 a { text-decoration: none; }
.product-card h3 a:hover { color: var(--green); }

.format-badge {
  flex-shrink: 0;
  padding: 5px 12px;
  border: 2px solid var(--edge);
  border-radius: 999px;
  font-weight: 900;
  font-size: 0.78rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--edge);
}

.format-badge.cup { background: var(--aqua); }
.format-badge.powder { background: var(--coral); }

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
  padding: 5px 11px;
  border: 2px solid var(--line);
  border-radius: 999px;
  font-weight: 800;
  font-size: 0.85rem;
  color: var(--ink);
  background: var(--bg-2);
}

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
  color: var(--yellow);
}

button.buy,
a.button,
button.primary {
  display: inline-flex;
  min-height: 42px;
  align-items: center;
  justify-content: center;
  padding: 8px 18px;
  border: 2px solid var(--edge);
  border-radius: 999px;
  background: var(--green);
  color: var(--edge);
  font: inherit;
  font-weight: 900;
  text-decoration: none;
  cursor: pointer;
  box-shadow: 4px 4px 0 var(--edge);
  transition: transform 120ms ease, box-shadow 120ms ease;
}

button.buy:hover,
a.button:hover,
button.primary:hover {
  transform: translate(2px, 2px);
  box-shadow: 2px 2px 0 var(--edge);
}

a.button.ghost { background: var(--card); color: var(--ink); }

button.danger { background: var(--coral); }

.card-status { min-height: 1.4em; font-weight: 800; font-size: 0.92rem; }
.card-status .added { color: var(--green); }
.card-status a { color: var(--aqua); }

.notice {
  max-width: 640px;
  padding: 16px 18px;
  border: 2px solid var(--edge);
  border-left: 10px solid var(--yellow);
  border-radius: 14px;
  background: var(--card);
  box-shadow: 5px 5px 0 var(--edge);
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
  border: 2px solid var(--edge);
  border-radius: 18px;
  background: var(--card);
  box-shadow: 7px 7px 0 var(--edge);
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
  border: 2px solid var(--line);
  border-radius: 10px;
  background: var(--bg-2);
  color: var(--ink);
  font: inherit;
}

.auth-card input:focus {
  outline: none;
  border-color: var(--aqua);
}

.auth-alt { color: var(--muted); font-size: 0.92rem; }
.auth-alt a { color: var(--aqua); }

.cart-table {
  width: 100%;
  border-collapse: collapse;
  border: 2px solid var(--edge);
  background: var(--card);
  box-shadow: 7px 7px 0 var(--edge);
  border-radius: 14px;
  overflow: hidden;
}

.cart-table th,
.cart-table td {
  padding: 12px 16px;
  text-align: left;
  border-bottom: 2px solid var(--line);
}

.cart-table th {
  background: var(--bg-2);
  font-size: 0.82rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--muted);
}

.cart-table tr:last-child td { border-bottom: none; }

.cart-total {
  margin-top: 18px;
  font-size: 1.25rem;
  font-weight: 950;
}

.cart-total strong { color: var(--yellow); }

.cart-actions { margin-top: 18px; display: flex; gap: 12px; flex-wrap: wrap; }

.site-footer {
  padding: 26px clamp(18px, 4%, 56px);
  border-top: 2px solid var(--line);
  background: var(--bg-2);
  color: var(--muted);
  font-weight: 700;
  display: flex;
  flex-wrap: wrap;
  gap: 10px;
  justify-content: space-between;
}

.site-footer .tagline { color: var(--green); font-weight: 900; }
"###;

pub fn format_price(cents: i64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// Shared document shell: dark athletic theme, htmx, header nav, footer.
pub fn layout(title: &str, user: Option<&AuthUser>, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover";
                meta name="theme-color" content="#0b1519";
                title { (title) }
                style { (PreEscaped(APP_CSS)) }
                script defer="defer" src="https://unpkg.com/htmx.org@2.0.4" {}
            }
            body {
                header .site-header {
                    a .brand-lockup href="/" {
                        span .brand-mark { "A-O" }
                        span .brand-name { "Athlet-O" }
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
                    span { "Athlet-O performance gelatin protein" }
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
        "Something wobbled wrong | Athlet-O",
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
                h3 { a href=(format!("/product/{}", product.slug)) { (product.name) } }
                span .format-badge.(format_class) { (product.format.label()) }
            }
            p .product-desc { (product.description) }
            div .stat-row {
                span { (product.protein_g) "g protein" }
                span { (product.calories) " kcal" }
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
        "Athlet-O | performance gelatin protein",
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
                "Not found | Athlet-O",
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
        &format!("{} ({}) | Athlet-O", product.name, product.format.label()),
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
