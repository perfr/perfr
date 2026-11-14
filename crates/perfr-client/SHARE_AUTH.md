# Share accounts

The viewer uploads a capture only after `GET /api/me` says who is signed in.
Paths and the JSON shape are constants in `src/share.rs` (`ME_PATH`,
`SHARE_PATH`, `LOGIN_PATH`) so they can move with the perfr-web accounts PR
without a hunt through the dialog.

`base` is `https://perfr.dev`, or `PERFR_SHARE_BASE` at build time, or
`?share_base=` when that value is an http(s) URL on localhost, `127.0.0.1`,
or `::1`.

## Contract

### `GET {base}/api/me`

The viewer sends `credentials: include`. When a login return provided a
bearer (see below) it also sends `Authorization: Bearer <token>`.

`200`:

```json
{
  "user": {"id": "u1", "name": "Ada Lovelace", "avatar": "https://…"},
  "quota": {"used_bytes": 0, "limit_bytes": 0, "max_capture_bytes": 0}
}
```

`avatar` may be empty. `name` may be empty; the dialog then shows `id`.

`401`:

```json
{"error": "login_required", "login_url": "/login?next=…"}
```

Any `401` is treated as signed out. `login_url` is optional. An absolute
`login_url` is used only when its origin is `base`; a path is joined onto
`base`. The buttons always add `provider` and `next` themselves:

```
{base}/login?provider=github|google&next=<percent-encoded viewer URL>
```

Other statuses are shown as a short error with Try again. Nothing is posted.

### Dialog

Signed out: the dialog says **Sign in to share** and offers GitHub and Google.
Each button opens the login URL in a popup (`window.name` is
`perfr-share-login`). If the browser blocks the popup, the same tab
navigates and Share reopens when it returns (`sessionStorage` key
`perfr.share.resume`).

Signed in: the dialog says **Sharing as {name}** and shows
`{used} of {limit} used · {remaining} left · up to {max} per capture`.
Upload is refused in the dialog when the bytes exceed `max_capture_bytes`
or the remaining quota (`limit_bytes - used_bytes`). A size equal to the
limit is allowed. The message tells the reader to share a selected range,
and that button is offered only when the range itself fits.

`POST {base}/api/share` (`Content-Type: application/octet-stream`) is sent
only after a fresh `GET /api/me` still says the bytes fit.

| Status | Dialog |
| --- | --- |
| 401 | Back to **Sign in to share**. "Your session expired. Sign in again to share." |
| 403 | Stay signed in. "You don't have permission to share this capture." |
| 413 | Stay signed in. "This capture is too large to share. Share a selected range to upload less." |
| 429 | Stay signed in. "Too many shares. Try again later." |

## Local viewer (cross-origin)

The native perfr service serves this viewer on loopback with
`Cross-Origin-Opener-Policy: same-origin` (SharedArrayBuffer). The share
host is perfr.dev. A popup to perfr.dev is a new browsing-context group, so
`window.opener` is null on the way back, and a perfr.dev session cookie is
third-party on the loopback page.

The flow that matches the cookie session, without a second device-code
protocol:

1. The button opens the login URL above. `next` is this viewer's URL
   (fragment stripped).
2. `GET /api/me` and `POST /api/share` use `credentials: include`
   (`xhr.withCredentials = true` on the upload).
3. When the popup is redirected back to the viewer, `index.html` runs
   before wasm. If `window.name` is `perfr-share-login` it writes
   `localStorage['perfr.share.logged_in']`, copies a `#share_token=` into
   `localStorage['perfr.share.token']`, `postMessage`s
   `{source: "perfr", type: "login"}` to the opener when that still
   exists (the same-origin case), and closes. A storage event reaches the
   opener across COOP browsing-context groups. `window.opener` does not.
4. While the signed-out dialog is up, the viewer also rechecks `/api/me`
   every couple of seconds, so a cookie that becomes visible resumes the
   share on its own.

### CORS headers perfr-web must send

On `GET /api/me`, `POST /api/share`, and the `OPTIONS` preflight, when
`Origin` is an allowed loopback viewer:

```
Access-Control-Allow-Origin: <the request Origin>
Access-Control-Allow-Credentials: true
Access-Control-Allow-Methods: GET, POST, OPTIONS
Access-Control-Allow-Headers: Content-Type, Authorization
Access-Control-Max-Age: 600
Vary: Origin
```

`Access-Control-Allow-Origin` is that specific origin, never `*`. Browsers
reject `*` on a credentialed request.

Allow only loopback viewers:

- `http://localhost:<port>`
- `http://127.0.0.1:<port>`
- `http://[::1]:<port>`

The viewer hosted on perfr.dev is same-origin and does not need these
headers. Do not reflect an arbitrary `Origin`. `POST /api/share` is
authenticated, and `Content-Type: application/octet-stream` is not a simple
content type, so the browser preflights. Rejecting that preflight is what
stops some other site from uploading with the user's session.

### Cookie

```
Set-Cookie: <session>=<value>; Path=/; HttpOnly; Secure; SameSite=None
```

`SameSite=None; Secure` is what lets the loopback page's credentialed fetch
include the cookie. `SameSite=Lax` is enough for the viewer hosted on
perfr.dev and is not enough for the local viewer.

Safari, Firefox (Total Cookie Protection), and Chrome with third-party
cookies blocked will not attach that cookie to a request from loopback. For
those browsers, append a one-time token to the login redirect as a
fragment. A fragment is not sent to the local perfr service:

```
{next}#share_token=<opaque>
```

The viewer stores it and sends `Authorization: Bearer <opaque>` on
`/api/me` and `/api/share`. Accept either the session cookie or that
bearer. The fragment name is `share_token` (`SHARE_TOKEN_FRAGMENT`).

### Login redirect

`/login` must honor `next` only when it is this site or a loopback viewer
URL, then redirect there after GitHub or Google sign-in. Reject every
other host.

### Why this and not a device code

The accounts service is a server-side session cookie on perfr.dev. A
device-code exchange would be a second protocol for the website PR to
implement and for this viewer to keep in step. The popup returns to the
`next` URL that PR already redirects to, and `/api/me` with
`credentials: include` is the session check either way. The
`#share_token=` fragment is the one addition that makes the local viewer
work where third-party cookies do not. Until the website sends it, Chrome
with the `SameSite=None` cookie above still shares, and the dialog says so
when a login return is still signed out.
