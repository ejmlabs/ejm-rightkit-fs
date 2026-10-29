# Browser checks and browser tests

Read when a change is visible in a page or an app window, when you capture before and after
screenshots, when you check a live deploy, or when you write or change a Playwright test.
`ejmlabs-design` holds the visual quality review; this file holds the verification steps.

## Look at the change

1. Run the repository's dev server or preview, as its AGENTS.md or verify skill says. If the
   preview lacks native features, it says so; check those in the native app.
2. Drive the page with the built-in browser, or with Playwright and the installed Chrome when you
   need an exact viewport.
3. Check light and dark. Set the theme the way the repository stores it before the page loads,
   so you see what a returning visitor sees.
4. Check the widths that matter: 320, 375, and 1440 for sites, 1280 for apps, plus any matrix the
   repository's design gate runs.
5. Confirm there is no horizontal overflow: `document.documentElement.scrollWidth` equals
   `clientWidth` at 320 and 375.
6. Tab through the changed controls. Focus stays visible and every action works from the keyboard.
7. Read the console. An error, or a 404 for one resource, is a real finding.

## Screenshots

- Capture the same flow before and after the change, in both themes. Show them to the user.
  Attach them to a PR only when the user asks.
- Keep screenshots and browser scripts in a scratch directory, never in the repository.
- A screenshot shows what you saw. It is not a test pass, and it does not cover a theme or width
  you did not capture.

## After a deploy

- Fetch the live URL with `curl -sI`: check the status and the security headers.
- Request a missing page and confirm it returns 404.
- Load the live page in a browser in both themes and read the console.
- Say which URL and which commit you checked.

## Playwright tests

- Wait on state, never on time. Use `expect(...)`, a response, or a URL. Do not call
  `wait_for_timeout` or `waitForTimeout`. The one exception is a wait that proves nothing arrives;
  name its constant and say why beside it.
- Find elements by role, label, or visible text first, a test id second, CSS last. Scope with a
  parent locator or a filter. Never use `first` or `nth` to silence a strict-mode error.
- Assert with `expect(locator)`, which retries. A bare `is_visible()` assertion reads the page once
  and races the render.
- Give each test its own workspace and accounts. Never rely on rows an earlier test left.
- After revoking a session, navigate to a protected page so the server must check again. A
  synthetic event alone does not prove the revoked session was refused.
- Run the repository's accessibility scan on each new screen state, and fail on serious or
  critical findings.
- Browser flows that a change touches run in the repository's browser test command before handoff.
