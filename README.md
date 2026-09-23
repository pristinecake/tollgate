# tollgate

A manifest-driven x402 paywall for HTTP APIs, written in Rust.

Point it at a [Kite service manifest](https://github.com/gokite-ai/kite-x402-services),
and it serves every endpoint the manifest declares behind HTTP 402. Unpaid
callers get a quote. Paid callers get the upstream response and a settlement
receipt, and only in that order.

There is no x402 SDK in this repository, because there is no x402 SDK for Rust.
The v2 wire format is implemented directly against the published schemas — the
whole protocol layer is `src/protocol.rs`, and it is small enough to read in one
sitting.

## What it does and does not charge for

Two invariants matter more than any feature, because they are the two ways a
paywall loses money:

- **A failed upstream is never charged for.** A 4xx or 5xx from the API being
  wrapped is the reason the caller should not pay. Payment is verified *before*
  the upstream is called and settled *after* it answers.
- **A failed settlement never releases the data.** If verification said yes and
  the transfer then did not land, the caller gets a 402 and no payload. Serving
  the body anyway is what turns a paywall into a free proxy.

Both are covered by end-to-end tests that stand up a real server, a real mock
facilitator, and a real mock upstream (`tests/gateway.rs`).

## Quick start

```bash
cargo build --release

# Validate the manifest, price every endpoint, and exit.
./target/release/tollgate --manifest service.yaml --check

# Serve it.
./target/release/tollgate --manifest service.yaml --listen 0.0.0.0:8080
```

`--check` is the one to put in a deploy pipeline. It fails on an unpriced
endpoint, an unknown network, a wallet that is not `0x` + 40 hex digits, or a
price the chain's token cannot express — rather than starting a service that
will 500 on its first paying call.

Configuration is environment-friendly, for containers:

| flag | environment | default |
|---|---|---|
| `--manifest` | `TOLLGATE_MANIFEST` | `service.yaml` |
| `--listen` | `TOLLGATE_LISTEN` | `0.0.0.0:8080` |
| `--facilitator` | `FACILITATOR_URL` | `https://facilitator.pieverse.io/v2` |
| `--upstream-auth-value` | `UPSTREAM_AUTH_VALUE` | unset |

## The manifest is the whole configuration

`service.yaml` in this repository is a real manifest for a real (free, keyless)
upstream — ECB reference exchange rates via
[Frankfurter](https://www.frankfurter.app/docs/). It is not a toy: it passes the
schema published by the official catalog, and the tests read the same values it
declares.

| endpoint | price | atomic units |
|---|---|---|
| `GET /v1/currencies` | 0.0005 USDC.e | 500 |
| `GET /v1/latest` | 0.001 USDC.e | 1000 |
| `GET /v1/timeseries` | 0.006 USDC.e | 6000 |

Adding an endpoint is a config change, not a code change. The router uses a
single fallback handler and consults the manifest's route table, so a path that
is not priced cannot be served by accident, and a path that *is* priced cannot
be served unpriced. The price list on `/healthz` is generated from the same
table, so the price a buyer reads and the price the server charges are
structurally incapable of disagreeing.

To check the manifest against the published schema:

```bash
npm install && npm run check:manifest
```

That is a Node script in a Rust repository on purpose: the schema is published
by a JavaScript toolchain, and the interesting claim — *a service configured
here can be submitted to the official catalog without being rewritten* — is only
worth anything if it is actually checked.

## How a paid call flows

```text
GET /v1/latest                      no payment header
  -> 402 + PAYMENT-REQUIRED          base64 quote, includes the URL you asked for
GET /v1/latest   PAYMENT-SIGNATURE  client signs the quote
  -> POST /verify                    is this spendable?
  -> GET  https://api.frankfurter.app/latest?from=EUR&to=USD
  -> POST /settle                    only if the upstream answered < 400
  -> 200 + PAYMENT-RESPONSE          the body, and the receipt
```

Three details that the diagram hides:

- The quote is issued without contacting the facilitator or the upstream. A
  price is local arithmetic.
- `X-PAYMENT` is accepted alongside `PAYMENT-SIGNATURE`. The v1 header name is
  still in circulation and refusing it turns a working client into a support
  ticket.
- When the facilitator is down, the caller gets a **502**, not a 402. Telling
  someone who did nothing wrong to pay again is how the same payment gets signed
  twice.

## Why the payment is compared locally

The facilitator verifies the signature, but it verifies it against whatever
`accepted` the client echoed back — and that is attacker-controlled. A client
can present a well-formed authorization for one atomic unit to its own address,
on a route that costs a thousandth of a dollar, and every signature check still
passes. `PaymentPayload::check_against` closes that: the server compares the
payment against the price it actually quoted, before any network call. The
mismatch is visible locally, so it is caught locally.

## Testing

```bash
cargo test        # 51 unit, 19 end-to-end, 6 manifest guards, 4 doc tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The end-to-end suite is the interesting one. It opens a socket, mounts a mock
facilitator and a mock upstream with `wiremock`, and drives the service the way
a client would: send a request, read the 402, satisfy it, send it again. Nothing
reaches into the paywall directly. Among other things it asserts that:

- a 503 from the upstream results in zero calls to `/settle`;
- a payment to the wrong wallet is refused without waking the facilitator;
- a settlement that fails does not put the payload on the wire;
- no header containing `payment` ever reaches the upstream, because a relayed
  authorization is a bearer instrument for the payer's money.

## Layout

```text
src/
  protocol.rs      x402 v2 wire types + base64 header codec
  facilitator.rs   HTTP client for /supported, /verify, /settle
  manifest.rs      parses and prices a Kite service.yaml
  paywall.rs       the decision logic — no web-framework types in this file
  app.rs           the axum router; one match arm per decision
  upstream.rs      relays the paid request, stripping the /v1 prefix
  chains.rs        Kite networks and their stablecoins
  money.rs         exact decimal-to-atomic conversion
  main.rs          CLI
tests/
  gateway.rs          end-to-end, over a real socket
  service_manifest.rs guards on the shipped service.yaml
schema/             the official service manifest schema, vendored
```

## Networks

| network | chain | token | decimals |
|---|---|---|---|
| `eip155:2366` | Kite mainnet | USDC.e | 6 |
| `eip155:2368` | Kite testnet | pieUSD | 18 |

The price is written once, in decimal, in the manifest. The integer that goes on
chain is derived per network at boot. A price a token cannot express is a
startup failure, not a silent truncation to zero.

## Status

Draft. The manifest points at a placeholder wallet and the upstream is a public
free API, so nothing here is charging anyone yet.

## License

Apache-2.0. See [LICENSE](LICENSE).
