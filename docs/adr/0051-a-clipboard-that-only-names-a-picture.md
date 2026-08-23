# 0051 — A clipboard that only names a picture

**Status:** accepted

## Context

`^V` attached a screenshot and did nothing at all with a picture copied off a web page. Reported the
way it is experienced: *"pasting an image works when I copy a file on disk, but if I copy an image
on the internet it doesn't."*

Both halves of that are exactly right, and they are two separate failures that end in the same
sentence — `nothing on the clipboard that is an image`.

**The clipboard was asked for one type and one type only.** `wl-paste --type image/png`, `xclip -t
image/png`. A clipboard is a *negotiation*: the program that copied says what it can provide, and
the program that pastes picks from that list. Asking for a single type is not picking from the list,
it is a guess — and it is wrong for a JPEG copied out of an image viewer, a WebP copied off a page,
and a BMP out of an X11 application, every one of which reads as an empty clipboard.

**And copying a picture off a page very often puts no picture on the clipboard at all.** What a
browser leaves is `text/html` — one `<img>` tag naming a URL — and sometimes a `text/uri-list` or
the plain URL beside it. Nothing but an address. Right-clicking and taking the address is the same
thing on purpose. There is no amount of asking the clipboard for bytes that finds those bytes,
because they are on somebody else's server and always were.

Two more things fell out of looking:

* A clipboard program that cannot reach a compositor at all — a workspace outlives the terminal
  that started it, so this is ordinary rather than exotic — failed exactly like an empty clipboard.
  The one problem nobody can guess from the outside was reported as the one everybody can.
* A photograph, once shrunk to 1568px, is several megabytes of PNG. It came back as *"more than a
  turn can carry"*, which is true of the encoding we chose and not of the picture.

## Decision

### Ask the clipboard what it has

`wl-paste --list-types`, `xclip -t TARGETS -o`. The best picture type *on offer* is the one
requested — PNG, then WebP, then JPEG, then GIF, then BMP — best rather than first, because a
program offering three has ranked nothing and the ranking is ours to make. The offered string is
what is asked for, capitals and parameters and all: `image/x-MS-bmp` is a real target atom and a
request for `image/x-ms-bmp` gets nothing back.

Only types the decoder here can open are asked for. Offering to take a thing we cannot read turns
"no picture" into "a picture that failed", which is worse. `image/svg+xml` is the notable absence.

macOS and Windows keep the try-each-program shape: their pasteboards cannot be asked cheaply, so the
programs *are* the question. macOS gained the one that matters for a picture copied off a page —
AppleScript can only hand over text, so `«class PNGf»` goes through a file and the path comes back.
`pngpaste` is still tried first and is still not installed by default.

### A picture the clipboard only names is fetched

`from_clipboard` answers with a `Clipped`: bytes it has already kept, or a `Remote` — an address for
somebody to go and get. Both are pictures; only one of them is here.

Where the address comes from, in the order it is trusted: `text/uri-list`, then the `src` of an
`<img>` in `text/html`, then a single line of `text/plain`. A `data:` URI is decoded rather than
fetched — small pictures on a page are often only that, and no network is involved.

Plain text is held to a stricter test than the HTML: an `<img>` tag has already said the thing it
points at is a picture, whereas a line of text is usually a line of text, so a URL there has to
*end* in a picture's extension before a key press will make a request to it. A clipboard holding an
ordinary link is not consent to fetch it.

The bytes that come back are sniffed like every other picture here. A URL ending in `.png` is a
filename, and filenames lie.

**The fetch does not happen on the host loop.** It is a round trip to a machine we have never met,
for as long as that machine feels like taking, and the loop is the single writer for the editor. It
is spawned, and it carries the conversation that was on screen when the key was pressed — switching
while a server takes its time is an ordinary thing to do, and the picture belongs where it was
asked for, not wherever you have got to. A conversation that has gone in the meantime is told so
rather than given a row nothing will ever draw.

It is also the one attachment that **says something on the way**. A chip appearing above the field
is the report everywhere else here, and it is enough — but a key press with several seconds of
nothing after it is a key press that did nothing, so a fetch says where it has gone and says again
when it arrives. `chat.attach` with nothing named is answered when the picture lands, not when the
clipboard was read: the plugin asked for a picture.

### A picture too busy to be a PNG is sent as a JPEG

PNG first, because most of what is pasted is a screenshot and a screenshot of text is where every
JPEG artefact lands on a letter. JPEG when the PNG will not fit, because a photograph is a
photograph. Refusing a picture over the encoding it happened to arrive in is the wrong answer twice.

Anything a decoder can open but no provider will take — a BMP off an X11 clipboard — is re-encoded
on the same path rather than refused.

### An empty clipboard is not a broken one

The type listing is the one place a clipboard program's stderr is repeated back, because it is the
one call that fails when the clipboard cannot be reached at all. Everything after it fails quietly
and ordinarily: a type that is not on offer is not an error. A program complaining that *nothing is
copied* is filtered out — that is the ordinary answer, and repeating it reads like something is
broken.

## Consequences

The workspace now makes an outbound request because of a key press. It is bounded — http and https
only, one request, no redirect chain worth mentioning, 20 seconds, 25MB — and it is exactly as
user-initiated as anything else here, but it is the first time `^V` has left the machine. A server
that answers with a 403 is a site declining to serve a picture to anything that is not its page,
which it is entitled to do; the number it said is reported as its answer rather than translated
into one of ours.

The clipboard is still read by the **workspace** rather than by the terminal viewing it, so ADR
0041's note stands and this makes it slightly sharper: a remote workspace would go and fetch the
right URL from the wrong machine.

`neosh` gained `reqwest` and `base64`, and `image` gained the `bmp` feature.
