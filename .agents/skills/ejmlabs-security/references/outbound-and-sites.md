# Outbound requests and site headers

Read before you send a request to a URL that a user, a model, a package, or stored settings can
influence, before you change a site's headers or CSP, and before you serve untrusted HTML.

## Outbound requests (SSRF)

- Where the destinations are known, allow only those hosts, and build the request from a fixed
  scheme, port, and path. A deny list of bad addresses is the last resort.
- When the host is user-chosen, use the repository's shared outbound client. It must:
  - require HTTPS, with no credentials and no fragment in the URL;
  - resolve the host, require every returned address to be public, and connect to that checked
    address while keeping the host name for SNI and certificate checks (a second lookup is open to
    DNS rebinding);
  - refuse private, loopback, link-local, metadata, and translated addresses, and alternate IP
    forms such as `2130706433` or `127.1`;
  - follow no redirect itself, and check each hop as a new request;
  - ignore proxy settings from the environment;
  - bound response size and time, and request identity encoding or cap the decoded size, so
    compression cannot slip past the bound.
- Apply the same checks to OAuth discovery, token endpoints, and their redirects.
- A destination inside a customer network needs an explicitly scoped runner, never the public path.
- A network check bounds where a request can go. It does not authorize sending the data there.
- Treat a development tunnel as public. It exposes every local route.

Tests: a private address is never requested, DNS rebinding after the first hop is refused, and
each redirect hop is checked again.

## Content Security Policy and headers

- Each site takes its CSP and security headers from the shared header source (the design system's
  site package or the repository's one header module). Never hand-edit a generated header file.
- The house baseline (`ejmlabs-cloudflare` holds the full header set) is `default-src 'self'`,
  `script-src 'self'`, `object-src 'none'`, `base-uri 'self'`, `form-action 'none'`, and
  `frame-ancestors 'none'`. Stricter is always allowed.
- Never put `'unsafe-inline'` or `'unsafe-eval'` in `script-src`. Use nonces only for a page that
  must keep inline script.
- Send the policy as an HTTP header. `frame-ancestors` and `sandbox` do not work in a `<meta>` tag.
- Loosen one directive only for a concrete need, owned by the shared header source, with the reason
  recorded beside it. Any other loosening is a finding.
- Serve user files and untrusted HTML with `Content-Security-Policy: sandbox`, in an opaque frame
  with no network.
- In server-rendered HTML, escape every user value. Serve scripts and styles from files, not inline.

## Check after a deploy

- Run the repository's header check in its gate when it has one.
- After a deploy, fetch the live URL with `curl -sI` and check the status and every security
  header. Then load it in a browser and read the console.
