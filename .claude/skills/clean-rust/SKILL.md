---
name: clean-rust
description: Read before defining a Rust struct, enum or trait; before adding a `.clone()`, `Arc`, `Box<dyn ...>` or lifetime to satisfy the borrow checker; before hand-writing a `poll` fn or a state `match` that returns "wrong state" errors; before adding `#![feature(..)]` or `unsafe`; and before reviewing any Rust diff. Covers validated newtypes, typestate / sealed traits / marker generics, lifetimes and Cow in structs plus Arc/Rc rules, poll-driven Future/AsyncWrite/AsyncRead state machines, the borrow-first rule that governs every .clone(), and the stop-and-ask procedure before relying on an unstable feature.
---

# Clean Rust: encode the invariant in the type

One idea, four applications: **if a rule has to hold, make the compiler the thing that holds it.**
Each section gives its decision rule (when the shape is mandatory, not merely nice), a skeleton that
compiles, and the misuse that now fails to compile. Later skeletons reuse earlier ones' types.

## Operating rules

### Unstable features: STOP, ASK, WAIT

Before writing code that needs a `#![feature(...)]` the crate does not already enable:

1. **Stop before writing it.** Do not write the workaround, do not write the nightly version.
2. Name the exact gate (`try_blocks`, `return_type_notation`, …) and its status on *this* toolchain.
   Gates get stabilised, renamed and removed — probe `#![feature(x)]` in a two-line scratch crate on
   the pinned channel before claiming a status.
3. Show both shapes side by side, in the smallest real form that carries the difference — the
   actual call site, not a toy.
4. State the cost honestly: on an already-nightly crate, one line in the crate root; **on a
   stable-pinned project**, one gate moves the crate and every source consumer to nightly, pins a
   nightly date in CI, and gives up the stable release path.
5. **Wait for an explicit yes or no.** No default, no timeout, no "stable shape plus a TODO".

The ask, verbatim shape:

```
Feature: `<gate>` — status on <channel>: <unstable | stable since X | already enabled in <crate>>
Clean shape (needs it):       <real code for the actual call site>
Stable shape (if you say no): <real code>
Cleaner because: <the invariant / allocation / duplication / boxing it removes>
Cost: <one #![feature] line in <crate>'s root, risk is a toolchain bump | crate is on stable, so
      enabling moves it and every source consumer to nightly, drops the MSRV, changes CI>
Enable it? (yes/no)
```

Exempt: gates already in that crate root's `#![feature(...)]` — use them freely.

No ask **and no nightly** when a nightly library method's stable equivalent is one line with
identical semantics: there is nothing to gain, so write the stable one. Any difference in shape or
semantics (fallible vs aborting allocation, different overflow behaviour) — ask.

### Cloning

Borrow first, in this order: `&T` / `&mut T` → a lifetime parameter on the struct → `Cow<'a, T>` →
iterator adaptors and `impl Iterator<Item = &'a T> + Clone` parameters (cloning an iterator is
cursors, not data) instead of `collect()` → `mem::take` / `mem::replace` / `Option::take` to move
out from behind `&mut` → `Arc::unwrap_or_clone(x)` when sole ownership is usually reclaimable (std,
stable — do not hand-roll `try_unwrap`/`unwrap_or_else`).

A `.clone()` that survives is **either** an `Arc`/`Rc`/`Weak` handle clone — nothing else counts as
obviously O(1), and `.clone()` on a `Copy` type is `clippy::clone_on_copy`, delete it — **or** it
carries a comment saying why the owned copy is required. A bare `.clone()` on a `String`/`Vec`/large
struct is a defect, and the usual cause is the *callee's* signature: a function taking `name: String`
that only passes `&name` onward has the wrong parameter — fix the signature, not the call site.

### Standing constraints

- No `unsafe` without explicit permission — ask, with the reason and the safe alternative you
  rejected. `Self: Unpin` bounds and pin-projection crates are safe; `Pin::new_unchecked`,
  `Pin::get_unchecked_mut` and `Pin::map_unchecked_mut` are `unsafe` and need the ask.
- `cargo clippy`, never `cargo check`. A new `#[allow(...)]` carries a comment with the tradeoff.
- Imports hoisted to a single `use` block at the top of the file; no inline `use` inside fns.
- No finding IDs, ticket markers, agent names, or AI/model references in code or comments.

## 1. Newtypes with validated constructors

**Mandatory** when a value has a rule a raw `String`/`Vec<u8>`/`u64`/`Uuid` cannot state: it crosses a
trust boundary (network, FFI, filesystem, user input); more than one function would otherwise re-check
it; or a downstream `unwrap`/index depends on it. **Optional** for a local, single-use value.

```rust
#[derive(Debug, PartialEq)]
pub enum NameError {
	Empty,
	Forbidden { ch: char, at: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name(String); // private field: no pub, no pub(crate)

impl Name {
	pub fn parse(s: &str) -> Result<Self, NameError> {
		if s.is_empty() {
			return Err(NameError::Empty);
		}
		if let Some((at, ch)) = s.char_indices().find(|(_, c)| *c == '/') {
			return Err(NameError::Forbidden { ch, at });
		}
		Ok(Self(s.to_owned()))
	}
}
```

- **One parser, every entrance.** `TryFrom`, `FromStr`, `Deserialize`, `FromSql`, an FFI `try_lift`
  all delegate to that one `parse`; a hand-written `Deserialize` that skips it reintroduces every
  invalid value the type exists to exclude.
- **Keep the proof.** Carry the newtype end to end; expose reads as `AsRef`/`Deref`. Validating then
  storing a `String` ("validate, throw the proof away") is a defect, as is a `pub`/`pub(crate)` field
  — it lets any sibling module skip the parser.
- **Errors are structured**, not strings: which rule broke plus the data that proves it (offending
  char and index, byte count). A caller must be able to `match`, not re-parse text.
- **When one authority mints the value, delete the constructor.** A server-assigned id gets no `new`,
  no `From<Uuid>`, no `FromStr` — only deserialization from that authority, plus a test-only cargo
  feature hatch if fixtures need one.
- **Derive nothing that leaks or forges**: hand-write `Debug` for secrets (print a digest); no
  `Default` unless the default is genuinely valid. **Prefer a std type that already carries the
  invariant**: `NonZeroU32` over `u32` + `assert!(n > 0)`, `[u8; 32]` over `Vec<u8>` + a length check.

**Fails to compile:** outside the module `Name("../etc".to_string())` →
``error[E0603]: tuple struct constructor `Name` is private``, and the struct-literal dodge
``Name { 0: s }`` → ``error[E0451]: field `0` of struct `Name` is private``.

## 2. Typestate, sealed traits, marker generics

### 2a. Sealing

**Mandatory** when a public trait's correctness depends on the finite set of implementors (exhaustive
dispatch, an invariant every impl must uphold, an associated type that must name an in-crate type).
**Optional** for a trait genuinely meant for downstream extension.

```rust
mod sealed {
	pub trait Sealed {}
}

pub trait Backend: sealed::Sealed + 'static {
	type Node: Clone + 'static;
	type Leaf: Clone + 'static;
}
```

A `pub(crate) trait Sealed` supertrait also seals but fires `private_bounds` (warn-by-default:
``trait `Sealed` is more private than the item `Backend` ``); use the private-module form in new code,
and where a crate already seals with `#[allow(private_bounds)]`, follow it and comment the allow.
Downstream code can still name the trait in bounds and call its methods, not implement it. Pair a
small **required** trait with a blanket-implemented **extension** trait
(`impl<T: Required + ?Sized> Ext for T {}`) for everything derivable.

A bound on an associated type is **not** a seal: `type Client: Private` only restricts which type an
impl may pick — any public type implementing `Private` lets a downstream `impl` through. Only a
supertrait the downstream crate cannot name closes the set.

### 2b. Marker generics

**Mandatory** when two values have the same Rust type but must never be interchanged (two backends,
two views of one store, two tenancy scopes) and mixing them is a bug the compiler could catch.
**Optional** when the distinction is a genuine runtime choice the code must `match` on — a configured
crypto version, a parsed protocol variant; do not typestate what must be inspectable at runtime.

```rust
use std::borrow::Cow;

pub struct Local; // ZST marker; impl sealed::Sealed + Backend for it

pub enum Item<'a, B: Backend + ?Sized> {
	Node(Cow<'a, B::Node>),
	Leaf(Cow<'a, B::Leaf>),
}
```

The marker is a ZST; `PhantomData<B>` carries it where no field mentions `B`. **Know the tax**: the
parameter and its bound thread through every type, alias and function that touches the value
generically. Pay it only when the mix-up is a real, reachable bug.

**Fails to compile:** a downstream `impl Backend for MyThing` (it cannot name `sealed::Sealed`),
and an `Item<'_, Remote>` passed where `Item<'_, Local>` is expected (E0308).

### 2c. Consuming state transitions

**Mandatory** when a wrong-phase call would corrupt state, lose data, or return `Ok` without doing
the work. **Optional** only when the wrong call returns an `Err` the caller already handles.

```rust
pub struct Draft(Vec<u8>);
pub struct Finalized(usize);

impl Draft {
	pub fn push(&mut self, bytes: &[u8]) {
		self.0.extend_from_slice(bytes);
	}
	pub fn finalize(self) -> Finalized {
		Finalized(self.0.len())
	}
}
```

One struct per phase, holding **only** that phase's fields — no `Option<T>` that is `Some` in three
phases and `None` in two, no field kept alive "just in case" (a lock or guard that must survive into
the next phase is exactly why it belongs to that phase's struct). Each transition is a method on the
*previous* phase taking `self` by value and returning the next.

**Fails to compile:** `d.push(b"x")` after `let f = d.finalize();` →
``error[E0382]: borrow of moved value: `d` ``.

A `match state { _ => Err("cannot X while Y") }` is the defect this replaces — except where the fact
is genuinely concurrent (another thread may drop the last handle mid-call) or a public API collapses
phases on purpose; say which in a comment at the definition.

## 3. Lifetimes and `Cow` in structs; `Arc`/`Rc`

Be willing to add `<'a>` to a struct — it is not an advanced move. **Mandatory** when a parsed view
points into a buffer the caller owns (parse into `&'a str` slices, never a `String` per segment), or
when one shape serves a hot borrowed path and a cold owned/stored path — then `Cow<'a, T>` plus one
escape hatch to `'static`:

```rust
use std::borrow::Cow;

pub struct Meta<'a> {
	name: Cow<'a, str>,
}

impl<'a> Meta<'a> {
	pub fn borrowed(name: &'a str) -> Self {
		Meta { name: Cow::Borrowed(name) }
	}

	pub fn into_owned(self) -> Meta<'static> {
		Meta {
			name: Cow::Owned(self.name.into_owned()),
		}
	}
}
```

Two near-identical `FooBorrowed` / `FooOwned` definitions are the defect this removes — they drift.
But `Cow` earns its place only when both a borrowed and an owned construction exist; a field only ever
built `Cow::Owned` is a `String` with extra syntax — defect. For zero-copy deserialization the
attribute is load-bearing: `#[serde(borrow)] name: Cow<'a, str>` — without it serde's blanket impl
always produces `Cow::Owned`. Offer `From<T>` (moves) alongside `From<&T>` (clones).

**Fails to compile:** returning `Meta::borrowed(&local)` from a fn declared `-> Meta<'static>` →
``error[E0515]: cannot return value referencing local variable``; storing it in a longer-lived
`let stored: Meta<'static>` → ``error[E0597]: `local` does not live long enough``. The only way out
is `into_owned()`, which the type spells out at the call site.

`Arc`/`Rc` only for one of these, named in a comment at the field or alias — except reason (3) when the
`spawn`/FFI export demanding `'static` is in the same function: (1) fan-out — several logical owners
must see one payload without copying it; (2) concurrent tasks must observe one piece of shared state
(with the interior mutability); (3) a `'static` boundary demands it (task spawn, FFI object, event
loop); (4) a lazily computed immutable value cached behind a lock, where cloning to escape the guard
must stay O(1).

Never for "the borrow checker complained". Payload shape matters: `Arc<str>` / `Arc<[T]>`, never
`Arc<String>` / `Arc<Vec<T>>` for a value immutable after construction — the inner growable buffer is
a second allocation and indirection buying nothing. If the crate targets both threaded and
single-threaded platforms, the `Send` bound and the `Arc`-vs-`Rc` choice live behind **one** cfg'd
alias pair in **one** module; a second hand-rolled cfg split at a call site is a defect.

## 4. Poll-driven state machines (`Future`, `AsyncWrite`, `AsyncRead`)

**Mandatory** when the operation has more than one await point and a phase-specific field set, or must
be named in an associated type (`Service::Future`) without boxing. **Optional** — and usually wrong —
when an `async fn` block says the same thing; hand-write only for the named type, a `Drop`-time
cancellation guard, or per-poll behaviour a block cannot express (a wrapper that merely decorates an
inner poll needs one `#[pin]` field and a delegating `poll`).

```rust
use std::{mem, pin::Pin, task::{Context, Poll}};

pub struct Collect<F> {
	state: State<F>,
}

enum State<F> {
	Awaiting(F),
	Draining { rest: Vec<u8> },
	Poisoned(&'static str),
}

impl<F> Future for Collect<F>
where
	F: Future<Output = Vec<u8>> + Unpin,
{
	type Output = Result<Vec<u8>, &'static str>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.as_mut().get_mut();
		loop {
			let taken = mem::replace(&mut this.state, State::Poisoned("polled after completion"));
			match taken {
				State::Awaiting(mut fut) => match Pin::new(&mut fut).poll(cx) {
					Poll::Ready(rest) => this.state = State::Draining { rest },
					Poll::Pending => {
						this.state = State::Awaiting(fut);
						return Poll::Pending;
					}
				},
				State::Draining { rest } => return Poll::Ready(Ok(rest)),
				State::Poisoned(why) => return Poll::Ready(Err(why)),
			}
		}
	}
}
```

- **One enum, one variant per phase**, each carrying only that phase's fields — not a flat struct of
  `Option`s jointly valid in some combinations only.
- **A terminal poison variant** so `mem::replace`/`Option::take` always has something cheap to leave
  behind — that is how an owned value gets out from behind `&mut self` with no `unsafe`.
- **On `Pending`, reinstall exactly what you polled** before returning; dropping an in-flight future
  or a partly filled buffer on `Pending` loses work silently. **Then loop** — after a transition,
  make progress in the same `poll` instead of parking for a re-poll.
- **Polled after `Ready` is a contract violation**: pick one of the two honest answers — a loud
  `panic!`/typed terminal error (as above), or `Fuse`-style permanent `Pending` named as such in the
  doc — and be consistent within the crate. Never a silent second `Ready`.
- **Pin without `unsafe`:** bound `Self: Unpin` (or the wrapped `S: Unpin`) and use `get_mut()` +
  `Pin::new(&mut field)`; pin-project only once a non-`Unpin` field must be polled in place.
- **`AsyncWrite` specifics:** `poll_write` returns `Ready(Ok(n))` with `n <= buf.len()`; `Ok(0)` for a
  non-empty `buf` means the sink accepts no more bytes — "not ready yet" is `Pending` with the waker
  registered. `poll_flush`/`poll_close` (tokio: `poll_shutdown`) survive repeated polls until they
  return `Ready`, then stay a cheap `Ready(Ok(()))` (close implies a flush).
- **`AsyncRead` specifics:** EOF is `Ready(Ok(0))` (futures) or `Ready(Ok(()))` with `buf.filled()`
  unchanged (tokio) — never use it for "nothing yet"; bytes copied into the buffer before a `Pending`
  are lost exactly like a dropped in-flight future.
- **Cancellation is `Option::take`, not a flag**: the completion path takes the sender/handle out, so
  a `Drop` finding `Some` knows it was cancelled in flight and cleans up, one finding `None` no-ops.
- **Name the future** rather than boxing; box only for heterogeneous collections or to unify
  differently shaped bodies across a `cfg` split. An optional async callback is a generic
  `F: FnOnce(..) -> Fut` plus a ZST always-ready default future behind a
  `type XDefault = X<fn(..) -> Noop, Noop>` alias, not `Option<Box<dyn FnOnce(..) -> BoxFuture>>`.

**Fails to compile:** keeping a copy of the buffer outside the state —
`this.state = State::Draining { rest }; return Poll::Ready(Ok(rest));` →
``error[E0382]: use of moved value: `rest` `` — and reaching for a phase's data from another phase —
`State::Awaiting { rest } => …` →
``error[E0769]: tuple variant `State::Awaiting` written as struct variant``.

## Review checklist

For each new type, name one invalid value it still admits and where that is caught; if the answer is a
runtime check in the caller, the type is wrong. Then — each line is a defect, not a preference:

1. Public/`pub(crate)` field on a type with a validating constructor.
2. A validated type unwrapped back into `String`/`Vec` and carried onward as raw data.
3. A construction path (`Deserialize`, `FromSql`, FFI lift, `FromStr`) that skips the parser.
4. An error carrying a formatted string where the caller needs to branch on the cause.
5. `assert!`/`if x == 0` guarding what `NonZeroU32`, a fixed array or an enum would make unrepresentable.
6. `match state { … => Err("cannot X in state Y") }` where a consuming transition fits, uncommented.
7. A phase struct holding a field only meaningful in another phase, or `Option` fields whose valid combinations live in prose.
8. A public trait whose implementor set must be closed but has no sealing supertrait — or one claiming a seal via an associated-type bound, which does not seal.
9. A marker parameter where the distinction never crosses a call boundary — or missing where two same-typed values get mixed.
10. `.clone()` on an owned heap value with no justifying comment and no O(1) handle.
11. A parameter taken by value that the body only borrows (forcing every caller to clone).
12. `Arc<String>`/`Arc<Vec<T>>` for an immutable payload, or an `Arc` whose reason is not one of the four (fan-out / shared task state / `'static` boundary / cached immutable).
13. Two near-identical owned and borrowed copies of one type instead of one `Cow` type — or a `Cow` field only ever built `Owned`.
14. A hand-written `Future`/`AsyncWrite` that drops in-flight work on `Pending`, has no terminal state, returns `Ok(0)` for "not ready", or reaches for `unsafe` where `Unpin` would do.
15. Any `#![feature(...)]` added without the stop-and-ask, or `unsafe` added without asking.
16. Inline `use` statements, `cargo check` in the workflow, an `#[allow]` with no stated tradeoff, or ticket/agent markers in code.
