# Grayboard: answer reviews from your phone

With a [Grayboard](https://github.com/nolnoch/grayboard) team server, a
review that waits for you sends you its questions, and your reply answers
them; and a review claims its change there, so a second review of it
doesn't start on any machine while the first runs or waits.

- **Your questions, in Slack.** When a review pauses at a gate, the daemon
  sends its questions to your `human` handle, which Grayboard's Slack app
  brings to your phone. Reply in the thread, a line per question: its
  number, then the option's number or name, such as `1: 1` and `2: Fix it`.
  Where a question takes your own words, write them after its number. The
  daemon checks your answers against the review and resumes it in the
  background, logging to its `progress.log`, and writes when it waits
  again or ends. A reply is read whole, however long, and found by its
  thread, so a later reply in the same Slack thread still reaches its
  review. A reply it can't read, or one to questions the review has moved
  past, gets an answer saying so. Answers that choose fixes for the
  review's builder session go to that session over the bus, for it to
  apply and fix.
- **A claim on the change.** `acr run` claims `review:<repository>#<pr>`,
  or `review:<repository>@<head>` with no pull request, and refuses to
  start when someone else holds it, naming them and since when. The daemon
  renews the claim while the review runs or waits, and releases it when
  the review is confirmed or stopped. A review that shows as running with
  no process driving it, because it crashed or was killed, keeps its claim
  for 10 minutes unrenewed, in case it comes back; then the claim goes. A
  merge or deploy hook can refuse
  while it's held: `grayboard claims --prefix 'review:myrepo#123'` exits 2
  when a claim matches.

Setting it up:

1. The server's operator registers acrbus as an OAuth client, in the
   server's `GRAYBOARD_OAUTH_CLIENTS`:
   `{"clientId":"acrbus","name":"acrbus review pipeline","redirectUris":["http://127.0.0.1/callback"],"scopes":["grayboard:tools"],"tokenEndpointAuthMethod":"none","allowRefresh":true,"sessionSource":true}`.
   `sessionSource` lets it bind a name and send messages as you. It asks
   only for `grayboard:tools`: it reads its inbox by polling and never
   opens delivery, which is all `grayboard:delivery` is for.
2. Sign in, in your browser: `acrbus grayboard login --server
   https://grayboard.example.com`. That adds `[grayboard]` to config.toml,
   with this machine's `name` written out.
3. Restart the daemon, `acrbus service install`, and check:
   `acrbus grayboard status` says the handle it holds.

| `[grayboard]` in config.toml | Is |
| --- | --- |
| `server` | The team server. Required. |
| `name` | The name this machine's acrbus binds under your account. Each machine needs its own, so the default is `acr-` and this machine's short host name, such as `acr-mini`, and sign-in writes it out. |
| `client_id` | The client the operator registered. Default `acrbus`. |
| `poll_secs` | How often the daemon checks for your answers. Default 20. |

A repository's config says how much goes there: `integrations: {grayboard:
nudge}` sends only "review X needs your answer: N questions", and you
answer at its machine; `full`, the default, sends the questions. Only the
daemon holds the sign-in and the session. Its files, the grant (`0600`),
the notes sent, your answers and the resumes' output, are under
`grayboard/` in the state folder, and the daemon's cleanup clears them
after 30 days. When the server is away, reviews go on without it: notes
wait to be sent, and a run that can't ask for its claim says so and
starts anyway. A note the server refuses, such as one to your `human`
handle before you've linked Slack, is said in the daemon's log and waits
longer before each try, while your replies and the claims go on.

The sign-in's refresh token rotates, and the server revokes the whole
sign-in if a spent one is ever sent again. So a refresh is marked before
it's sent, and one whose outcome is unknown, because its answer was lost
or acrbus stopped mid-refresh, is never sent again: its token is
revoked, or queued for the next sign-in or sign-out to revoke, and
`acrbus grayboard status` says to sign in again.
