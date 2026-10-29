# Auth and credentials

Read before you change sign-in, sessions, cookies, browser writes, passwords, OAuth, token
renewal, logout, or credential storage. The product's auth package owns these. Change it there,
not in a host.

## Ownership

- One package per product owns sign-in, sessions, renewal, and credential storage: the shared auth
  crate for the harness and its hosts, or a maintained library such as Better Auth for services.
- A host owns only what the user sees, the browser launch, and the loopback listener.
- Keep OAuth primitives, JWT checks, HTTP parsing, and TLS in maintained libraries. Never write
  them by hand.
- Never collect, copy, or reuse credentials that belong to another vendor's official client.

## Sessions and cookies

- A session token has at least 128 random bits. Store only its hash. Issue a new token at each
  sign-in and re-authentication.
- End the session on the server at logout, account disable, and credential change. Deleting the
  cookie is not enough. Set and document an idle timeout and an absolute lifetime.
- Cookies are `Secure`, `HttpOnly`, an explicit `SameSite`, the `__Host-` prefix, no `Domain`, and
  `Path=/`. Use `Lax` only where a cross-site redirect must carry the cookie, with a comment why.
- A browser write needs a custom request header and an exact `Origin` match. A missing or `null`
  origin is never same-origin. No write on GET. `SameSite` is defense in depth, not the check.

## Passwords and secret comparison

- Prefer the auth library's password support. Otherwise require at least 15 characters for a
  single-factor password, accept long ones, and set no composition rules, no forced rotation, and
  a blocklist of known-bad values.
- Hash with a memory-hard function (scrypt or Argon2id at the OWASP cost settings) and keep the
  parameters with the hash, so a cost change can rehash on the next sign-in.
- Count an attempt before the slow hash runs, and throttle guesses with shared state. A missing
  account takes the same time as a wrong password.
- Compare tokens, signatures, and webhook secrets in constant time. Check a webhook's timestamp
  window as well as its signature.

## OAuth and sign-in flows

- Use the authorization code flow with PKCE `S256`. Never fall back to `plain`.
- Bind every attempt to fresh PKCE, state, and nonce. State is single use and bound to one browser.
- Match redirect URIs exactly, and take them from configuration, never from the request.
- With more than one issuer, check the `iss` response parameter against discovery metadata.
- Validate signature, issuer, audience, and nonce before you replace stored credentials.
- Browser launch URLs never carry tokens or token hints. Validate the authorization URL's origin
  before you show or open it.
- A loopback sign-in binds its listener first, then opens the browser. Accept a callback only on the
  exact host and path, answer anything else without echoing the code, time out, honor Ctrl+C, and
  release the port on every path.
- With several saved accounts, require an explicit account choice. Never guess.

## Storage, renewal, and logout

- Store credentials in the shared vault. The OS key service or a managed key store holds only the
  key. Never fall back to plaintext and never silently reset a missing key. Memory-only storage
  must be explicitly selected.
- Bind each encrypted credential to its scope: the account, the workspace, or the exact HTTPS origin
  it may be sent to. Keep key versions and a tested rotation and revocation path.
- Serialize renewal across every process that shares the storage. After a crash during a refresh
  whose outcome is uncertain, recover; never retry an old rotating token.
- Logout clears local credentials even when remote revocation cannot be confirmed, and says so.
- Read provider keys from the environment. Never accept a token as a command argument.

## Tests

- Cover invalid state, nonce, signature, and audience, a changed identity, a rotation race,
  interrupted persistence, and redacted errors for the flow you change.
- Use local fixtures and the established JWT library for signed identity fixtures. Never sign in
  against a real provider from a test.
- Add a test where a member of another tenant gets a not-found or forbidden answer.
