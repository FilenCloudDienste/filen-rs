//! SVG via `resvg` (usvg + tiny-skia), built with every default feature off:
//! no text (no font database — text elements render as nothing, which beats
//! shipping megabytes of fonts to a mobile binary), no raster-images, no
//! system-font or filesystem access, no gzip — `.svgz` (magic `1f 8b`) never
//! matches the sniff and is refused on purpose, since supporting it would buy
//! an inflate dependency plus an unbounded-decompression guard for a format
//! nobody uploads.
//!
//! Being a vector format, the raster size is OURS to pick: `open` derives the
//! aspect from the root tag (`width`/`height` when absolute, else `viewBox`,
//! else square) and commits to a raster covering the — already budget-clamped
//! — target. Unlike the raster formats a small nominal size is still rendered
//! at the target: scaling up is what vectors are for.
//!
//! The document is an untrusted XML file, and the hostile cases are not
//! memory-shaped in the usual way, so the guards are explicit:
//! - roxmltree (under usvg) refuses external entities and bounds internal
//!   entity expansion (depth 10, 255 refs), but expansion is still quadratic
//!   in document size — any `<!ENTITY` declaration is refused outright.
//! - usvg resolves `<use>` by deep-copying the referenced subtree, and copies
//!   a `<marker>` subtree once per shape vertex — both during the parse, with
//!   no bound short of its 1M-node backstop (hundreds of MB, far beyond any
//!   budget here; and marker copies are made in a later stage that the
//!   backstop never sees at all). A memoised pre-pass costs the expansion from
//!   the raw XML and refuses before `usvg::Tree::from_str` sees the document.
//!   The marker charge lives INSIDE the per-node cost so that it multiplies
//!   through `<use>` the way usvg does; a marker that could carry another
//!   marker turns that product into a chain with no fixed point and is
//!   refused outright.
//! - resvg sizes a `<pattern>` tile pixmap from the pattern rect with no
//!   clamp at all: `<pattern>` documents are refused.
//! - tiny-skia builds up to a million dash segments per path before giving up
//!   on a `stroke-dasharray`, which a 200-byte document can reach; the dash
//!   count is estimated from the parsed path and charged to the same render
//!   allowance as the layers.
//! - both image href resolvers are overridden to answer `None`: the default
//!   string resolver reads local files, and nothing here may touch the
//!   filesystem or decode embedded rasters.
//! - the XML layer recurses per nesting level and a deep enough document
//!   overflows the decode thread's stack — an ABORT, not a catchable panic —
//!   so nesting is counted in the byte scan and refused past 1024, matching
//!   usvg's own (too-late) cap.
//! - filter primitives cost CPU and region-sized buffers per primitive, so
//!   their count is capped; isolated-group layers (opacity, clip, mask,
//!   filter) each allocate a sub-pixmap at render time, so a fixed allowance
//!   for them is priced into every estimate and the parsed tree's worst
//!   concurrent stack — each layer clamped the way resvg clamps it — is held
//!   to that allowance before rendering starts.
//!
//! The scans work on the raw text and count a match inside a comment or CDATA
//! block as real — refusing a weird-but-benign document is acceptable, missing
//! a hostile one is not.

use std::collections::HashMap;

use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SmallImage, ThumbError, ThumbSpec,
};

use resvg::{tiny_skia, usvg};

pub struct Svg;

/// Whole-document ceiling: parse work is at least linear in the text, and the
/// text is held for the whole decode. Real SVGs above this are vanishingly
/// rare (huge exported maps), and the budget check would refuse most of them
/// anyway — this cap exists for callers with big budgets, and to bound what
/// `open` reads before any budget is consulted.
const MAX_SVG_DOC_BYTES: u64 = 2 * 1024 * 1024;

/// Filter primitives each burn CPU over their region and allocate
/// region-sized scratch; no sane document carries more than a handful.
const MAX_FILTER_PRIMITIVE_TAGS: usize = 64;

/// Element-nesting ceiling, matched to usvg's own (`parse_xml_node` answers
/// `NodesLimitReached` past depth 1024). Ours has to fire FIRST, from the
/// cheap byte scan: the XML layer underneath usvg recurses per level and
/// overflows the 2 MiB stack a `spawn_blocking` decode thread gets at a depth
/// well under 1024's worth of frames — and a stack overflow ABORTS the
/// process rather than unwinding, which on iOS takes the whole file-provider
/// extension down with it. Documents usvg would have accepted are unaffected.
const MAX_ELEMENT_DEPTH: usize = 1024;

/// Ceiling on the nodes a document may expand to once every `<use>` is
/// resolved and every marker copied — the currency the expansion pre-pass
/// counts in. Generous next to any hand- or tool-authored file (a busy
/// Illustrator export is a few thousand elements) and small next to what usvg
/// would otherwise happily materialise.
const MAX_EXPANSION_NODES: u64 = 100_000;

/// Ceiling on the path segments one built shape (`rect`, `circle`, `ellipse`)
/// may flatten into. usvg struck their corners as arcs and kurbo subdivides an
/// arc by its RADIUS, so `<circle r="1e30"/>` — four characters of radius —
/// becomes millions of segments and tens of megabytes with no `<use>` and no
/// marker in sight. Four thousand is beyond any drawing and still leaves
/// radii up to ~4·10²⁰ renderable.
const MAX_SHAPE_ARC_VERTICES: u64 = 4096;

/// Reference hops — a `mask`, `clip-path`, `filter` or paint link, an `href`
/// on anything but a `<use>`, or the `<use>` link itself — one resolution may
/// chain through.
///
/// usvg resolves each of those RECURSIVELY, so a chain of them is a stack
/// depth: 5000 chained `<mask>`s — 400 KB of perfectly acyclic document —
/// aborted the process, and the abort threshold on the 2 MiB stack a
/// `spawn_blocking` decode thread gets sits between 400 and 800 links. Real
/// documents chain a handful at most, so this is set an order of magnitude
/// below what survives rather than at the edge of it.
///
/// The `<use>` link is capped here rather than more loosely because it is the
/// same recursion: usvg deep-copies a `<use>` target and resolves the `<use>`
/// inside that copy from within the copy. The trade-off is real — a document
/// composing 33 layers of `<use>` renders fine today and is now refused — but
/// nothing exports that way, and the alternative was the accidental one below.
const MAX_REFERENCE_HOPS: u32 = 32;

/// Nodes charged for one element of the largest `<filter>`, per element that
/// filter can land on.
///
/// usvg copies the RESOLVED filter chain into every group it applies to, so
/// the chain multiplies through `<use>` exactly the way a marker does. One
/// copied primitive measured ~336 B against the 256 B `peak_estimate` prices
/// a node at, hence two nodes rather than one.
const FILTER_NODE_WEIGHT: u64 = 2;

/// Worst-case concurrent isolated-layer bytes `decode_into` will accept, in
/// units of one PADDED output pixmap ([`layer_unit_bytes`]) — priced into
/// `peak_estimate` for EVERY document and enforced against the real tree
/// before rendering.
///
/// Sized for what real files carry: a full-canvas `clipPath` — which is in
/// essentially every Figma/Illustrator/Inkscape export — costs three pixmaps
/// on its own (the layer, the clip pixmap, its mask), and nested opacity
/// groups one each. Charging it unconditionally is the only honest option: a
/// `<style>` block can put `opacity` on every element in the document, so no
/// scan of the text or the attributes can tell in advance whether a document
/// has layers.
const LAYERED_ALLOWANCE_PIXMAPS: usize = 6;

/// Whole pixels resvg adds to an isolated layer before allocating its
/// sub-pixmap: it rounds the bounding box out and leaves slop on each side.
///
/// This is an ABSOLUTE cost — it does not shrink with the target — so both
/// sides of the allowance have to quote it. Charging it in `layer_peak` while
/// pricing the allowance in unpadded pixmaps made the same document pass or
/// fail on target size alone: it is ~13% of a 64 px canvas but ~1.6% of a
/// 512 px one, so an export sitting near the line was refused small and
/// rendered large.
const LAYER_PAD: u32 = 4;

impl FormatDecoder for Svg {
	fn detect(&self, prefix: &[u8]) -> bool {
		// Registered after every binary format: this is a text sniff and must
		// never outrank a magic number.
		root_svg_offset(prefix).is_some()
	}

	fn open(
		&self,
		mut src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let len = src.len();
		if len == 0 || len > MAX_SVG_DOC_BYTES {
			return Err(ThumbError::Decode(format!(
				"svg: {len} bytes is outside the supported document size"
			)));
		}
		// No header/payload split exists in XML — the whole (capped) document
		// is the header, and usvg needs all of it anyway.
		let mut text = vec![0u8; len as usize];
		let mut filled = 0;
		while filled < text.len() {
			let n = src.read_at(filled as u64, &mut text[filled..])?;
			if n == 0 {
				return Err(ThumbError::Decode("svg: source ended early".into()));
			}
			filled += n;
		}
		let scan = scan_document(&text)?;
		let root = root_svg_offset(&text).ok_or_else(|| {
			ThumbError::Decode("svg: root element vanished between sniff and open".into())
		})?;
		let aspect = root_aspect(&text, root);
		let out_dims = raster_dims(aspect, spec);
		let text = String::from_utf8(text)
			.map_err(|_| ThumbError::Decode("svg: document is not utf-8".into()))?;
		let mut prepared = PreparedSvg {
			text,
			out_dims,
			tag_count: scan.tags,
			attr_count: scan.attrs,
			effective_nodes: 0,
		};
		// The expansion pre-pass parses the document a second time, so it runs
		// only once the cheap estimate has cleared the budget: a document that
		// cannot be afforded is refused by `decode_bounded` without ever
		// reaching `decode_into`, and the tree the pre-pass builds is smaller
		// than the per-tag charge the estimate just cleared.
		if prepared.peak_estimate() <= spec.mem_budget {
			prepared.effective_nodes = expansion_cost(&prepared.text)?;
		}
		Ok(Box::new(prepared))
	}
}

/// Byte offset of the root start tag's `<` when the prologue is XML-shaped
/// (optional BOM, whitespace, `<?…?>`, comments, DOCTYPE) and the root
/// element's local name is `svg`. `None` when the input runs out before the
/// root element — an unconfirmed document is not claimed.
fn root_svg_offset(bytes: &[u8]) -> Option<usize> {
	let mut i = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
		3
	} else {
		0
	};
	loop {
		while bytes.get(i)?.is_ascii_whitespace() {
			i += 1;
		}
		let rest = &bytes[i..];
		if rest.starts_with(b"<?") {
			i += 2 + find(&rest[2..], b"?>")? + 2;
		} else if rest.starts_with(b"<!--") {
			i += 4 + find(&rest[4..], b"-->")? + 3;
		} else if rest.starts_with(b"<!") {
			// DOCTYPE — its internal subset may hold `>` inside `[…]`.
			let mut depth = 0u32;
			let mut j = i + 2;
			loop {
				match bytes.get(j)? {
					b'[' => depth += 1,
					b']' => depth = depth.saturating_sub(1),
					b'>' if depth == 0 => break,
					_ => {}
				}
				j += 1;
			}
			i = j + 1;
		} else if rest.starts_with(b"<") {
			let name_start = i + 1;
			let mut j = name_start;
			loop {
				let b = bytes.get(j)?;
				if b.is_ascii_whitespace() || matches!(b, b'>' | b'/') {
					break;
				}
				j += 1;
			}
			let name = &bytes[name_start..j];
			let local = name.rsplit(|b| *b == b':').next().unwrap_or(name);
			return (local == b"svg").then_some(i);
		} else {
			return None;
		}
	}
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|w| w == needle)
}

struct DocScan {
	/// `<` occurrences — an upper bound on XML nodes, for pricing the parse.
	tags: usize,
	/// `=` occurrences inside tags — an upper bound on attributes, same purpose.
	attrs: usize,
}

/// Walks a tag from `from` (already past its name) to just past its `>`,
/// honouring quoted attribute values so a `>` inside one does not end it.
/// Answers where to resume, whether the tag closed itself (`/>`), and how many
/// `=` it carried.
fn scan_tag(bytes: &[u8], from: usize) -> (usize, bool, usize) {
	let mut i = from;
	let mut attrs = 0usize;
	let mut quote = 0u8;
	let mut last = 0u8;
	while i < bytes.len() {
		let b = bytes[i];
		if quote != 0 {
			if b == quote {
				quote = 0;
			}
		} else {
			match b {
				b'"' | b'\'' => quote = b,
				b'=' => attrs += 1,
				b'>' => return (i + 1, last == b'/', attrs),
				_ => {}
			}
		}
		if !b.is_ascii_whitespace() {
			last = b;
		}
		i += 1;
	}
	(bytes.len(), false, attrs)
}

/// One pass over the raw text: refuses the constructs documented in the
/// module docs and counts what `peak_estimate` prices. Tag names are matched
/// case-sensitively because XML is — `<USE>` is not a `use` element to usvg
/// either.
///
/// Walks tag by tag rather than byte by byte: element nesting has to be
/// tracked (see [`MAX_ELEMENT_DEPTH`]), which needs comments and CDATA skipped
/// so a `</g>` inside one cannot forge a decrement. Declarations (`<!DOCTYPE`
/// and friends) are stepped over one byte at a time on purpose, so a
/// `<!ENTITY` nested in a DOCTYPE's internal subset is still seen.
fn scan_document(bytes: &[u8]) -> Result<DocScan, ThumbError> {
	let mut scan = DocScan { tags: 0, attrs: 0 };
	let mut filter_primitives = 0usize;
	let mut depth = 0usize;
	let mut i = 0usize;
	while let Some(off) = bytes[i..].iter().position(|b| *b == b'<') {
		// Past the `<`, at the first byte of whatever it opens.
		i += off + 1;
		scan.tags += 1;
		let rest = &bytes[i..];
		if rest.starts_with(b"!--") {
			i = find(rest, b"-->").map_or(bytes.len(), |e| i + e + 3);
			continue;
		}
		if rest.starts_with(b"![CDATA[") {
			i = find(rest, b"]]>").map_or(bytes.len(), |e| i + e + 3);
			continue;
		}
		if rest.starts_with(b"!ENTITY") {
			return Err(ThumbError::Decode(
				"svg: entity declarations are refused — expansion is quadratic in the document"
					.into(),
			));
		}
		if rest.starts_with(b"?") {
			i = find(rest, b"?>").map_or(bytes.len(), |e| i + e + 2);
			continue;
		}
		if rest.starts_with(b"!") {
			// A declaration, not an element: leave the rest to the loop.
			continue;
		}
		let closing = rest.starts_with(b"/");
		let name_start = if closing { i + 1 } else { i };
		let mut j = name_start;
		// Runs to the name's own terminator rather than to a fixed window.
		// The window used to stop at 64 bytes to keep a delimiter-free
		// document from turning this pass quadratic — but it never could:
		// `scan_tag` below resumes wherever this stops and the loop then sets
		// `i` past the whole tag, so every byte is walked a bounded number of
		// times either way. What the window did do was truncate the element
		// name, so a namespace prefix long enough to push the local name out
		// of it hid the element from the refusals below — while usvg, which
		// resolves by namespace URI rather than by prefix, still saw it for
		// what it was.
		let mut local_start = name_start;
		while j < bytes.len() && !bytes[j].is_ascii_whitespace() && !matches!(bytes[j], b'>' | b'/')
		{
			if bytes[j] == b':' {
				local_start = j + 1;
			}
			j += 1;
		}
		let local = &bytes[local_start..j];
		let (end, self_closing, tag_attrs) = scan_tag(bytes, j);
		i = end;
		if closing {
			depth = depth.saturating_sub(1);
			continue;
		}
		scan.attrs += tag_attrs;
		depth += 1;
		if depth > MAX_ELEMENT_DEPTH {
			return Err(ThumbError::Decode(
				"svg: element nesting is deeper than the parser can survive".into(),
			));
		}
		if self_closing {
			depth -= 1;
		}
		if local == b"pattern" {
			return Err(ThumbError::Decode(
				"svg: <pattern> is refused — resvg sizes its tile pixmap from the pattern \
				 rect, unclamped"
					.into(),
			));
		}
		// Only start tags count: a non-self-closing `<feBlend>…</feBlend>` is
		// one primitive, not two. CSS filter FUNCTIONS (`filter="blur(2)"`)
		// carry no `fe*` tag at all and are deliberately not counted here —
		// the isolated-layer pricing is what backstops those.
		if local.starts_with(b"fe") {
			filter_primitives += 1;
			if filter_primitives > MAX_FILTER_PRIMITIVE_TAGS {
				return Err(ThumbError::Decode("svg: too many filter primitives".into()));
			}
		}
	}
	Ok(scan)
}

/// Colours for the pre-pass' cycle detection: unvisited, on the resolution
/// stack (a second visit is a cycle), resolved.
const WHITE: u8 = 0;
const GREY: u8 = 1;
const BLACK: u8 = 2;

/// One element being costed, and what it still owes: its element children,
/// plus the subtree a `<use>` points at.
struct Frame<'a, 'i> {
	node: roxmltree::Node<'a, 'i>,
	children: roxmltree::Children<'a, 'i>,
	link: Option<roxmltree::Node<'a, 'i>>,
	cost: u64,
	/// Whether the parent frame reached this one through its `<use>` link, so
	/// the parent knows to charge a hop when this frame's count folds back in.
	via_link: bool,
	/// Longest `<use>` chain rooted at this element, in link hops. Containment
	/// carries the maximum up unchanged: nesting is bounded separately, by
	/// [`MAX_ELEMENT_DEPTH`] in the byte scan, and capping it at a reference
	/// chain's length would refuse ordinary deeply-grouped exports.
	hops: u32,
}

impl<'a, 'i> Frame<'a, 'i> {
	fn new(
		node: roxmltree::Node<'a, 'i>,
		link: Option<roxmltree::Node<'a, 'i>>,
		cost: u64,
		via_link: bool,
	) -> Self {
		Frame {
			node,
			children: node.children(),
			link,
			cost,
			via_link,
			hops: 0,
		}
	}

	/// The next element this one reaches, and whether it was reached through
	/// the `<use>` link — which costs a usvg recursion level — or by
	/// containment, which does not.
	fn next_dep(&mut self) -> Option<(roxmltree::Node<'a, 'i>, bool)> {
		if let Some(link) = self.link.take() {
			return Some((link, true));
		}
		self.children
			.by_ref()
			.find(roxmltree::Node::is_element)
			.map(|child| (child, false))
	}
}

const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// The `<use>` target for an element, if it names one. `href`/`xlink:href`
/// only — usvg resolves the link from the attribute, never from CSS — and only
/// same-document fragments, since nothing here may fetch a file.
///
/// Resolved exactly the way usvg's `resolve_href` does, because anything this
/// misses is expansion it does not charge for. roxmltree matches an attribute
/// by LOCAL name, so the namespace has to be checked here: SVG 2 gives an
/// unprefixed `href` precedence over `xlink:href` whatever their order, and an
/// `href` in any other namespace is not a link at all. The value is an
/// `svgtypes::IRI`, which skips surrounding whitespace before the `#` — and
/// XML attribute-value normalisation turns the newline in a pretty-printed
/// `href="\n  #icon"` into exactly that.
fn use_target<'a, 'i>(
	node: roxmltree::Node<'a, 'i>,
	targets: &HashMap<&'a str, roxmltree::Node<'a, 'i>>,
) -> Option<roxmltree::Node<'a, 'i>> {
	if node.tag_name().name() != "use" {
		return None;
	}
	let href = node
		.attributes()
		.find(|a| a.name() == "href" && a.namespace().is_none())
		.or_else(|| {
			node.attributes()
				.find(|a| a.name() == "href" && a.namespace() == Some(XLINK_NS))
		})?
		.value();
	targets.get(href.trim().strip_prefix('#')?).copied()
}

/// Attributes usvg resolves a same-document `url(#id)` through, and recurses
/// into. `href`/`xlink:href` is handled beside them, and only off a `<use>`:
/// that edge is an EXPANSION, which `use_target` follows and charges for.
const FUNC_IRI_ATTRS: [&str; 5] = ["mask", "clip-path", "filter", "fill", "stroke"];

/// The id inside a `FuncIRI` (`url(#id)`), read the way `svgtypes` reads one:
/// leading whitespace skipped, the id optionally quoted, running to the quote
/// or the `)`.
fn func_iri(value: &str) -> Option<&str> {
	let rest = value.trim_start().strip_prefix("url(")?.trim_start();
	let quoted = rest.starts_with(['\'', '"']);
	let rest = if quoted { &rest[1..] } else { rest };
	let id = rest.trim_start().strip_prefix('#')?;
	let end = id
		.find([')', '\'', '"', ' ', '\t', '\n', '\r'])
		.unwrap_or(id.len());
	(end > 0).then(|| &id[..end])
}

/// The id inside an `IRI` (`#id`), likewise: `svgtypes` skips leading spaces
/// and stops the id at the first one.
fn iri(value: &str) -> Option<&str> {
	let id = value.trim_start().strip_prefix('#')?;
	let id = id.split([' ', '\t', '\n', '\r']).next().unwrap_or(id);
	(!id.is_empty()).then_some(id)
}

/// One element on the reference walk, and what it still reaches: the elements
/// its attributes point at, then its element children.
struct RefFrame<'a, 'i> {
	node: roxmltree::Node<'a, 'i>,
	on_use: bool,
	attrs: roxmltree::Attributes<'a, 'i>,
	children: roxmltree::Children<'a, 'i>,
}

impl<'a, 'i> RefFrame<'a, 'i> {
	fn new(node: roxmltree::Node<'a, 'i>) -> Self {
		RefFrame {
			node,
			on_use: node.tag_name().name() == "use",
			attrs: node.attributes(),
			children: node.children(),
		}
	}

	/// The next element this one reaches, and whether it was reached by a
	/// REFERENCE (which costs a usvg recursion level) or by containment.
	fn next_dep(
		&mut self,
		targets: &HashMap<&'a str, roxmltree::Node<'a, 'i>>,
	) -> Option<(roxmltree::Node<'a, 'i>, bool)> {
		for attr in self.attrs.by_ref() {
			let id = if FUNC_IRI_ATTRS.contains(&attr.name()) {
				func_iri(attr.value())
			} else if attr.name() == "href"
				&& !self.on_use
				&& matches!(attr.namespace(), None | Some(XLINK_NS))
			{
				iri(attr.value())
			} else {
				None
			};
			if let Some(target) = id.and_then(|id| targets.get(id)) {
				return Some((*target, true));
			}
		}
		self.children
			.by_ref()
			.find(roxmltree::Node::is_element)
			.map(|child| (child, false))
	}
}

/// Refuses a document whose REFERENCE graph loops, or chains deeper than
/// [`MAX_REFERENCE_HOPS`].
///
/// The expansion walk follows exactly one same-document edge — `href` on
/// `<use>`. Every other one (`mask`, `clip-path`, `filter`, the paint servers,
/// `href` elsewhere) reached `usvg::Tree::from_str` with no cycle check at
/// all, and usvg's own defence is partial: `fix_recursive_links` walks a
/// node's descendants plus ONE hop, which neutralises self-links and 2-cycles
/// and nothing longer, and `mask.rs` memoises a mask only AFTER converting it,
/// so nothing shortens the recursion either. A 285-byte three-`<mask>` cycle
/// died with `fatal runtime error: stack overflow` — an abort, not a catchable
/// panic, which on iOS takes the whole file-provider extension with it — the
/// same three `<clipPath>`s did the same, and a gradient chain `A→B→C→B` spun
/// at 100% CPU without ever returning, because usvg's `HrefIter` rejects only
/// the first and current node and misses cycles among the ones between.
///
/// Cycles ONLY: nothing is costed here. usvg memoises a mask, clip or gradient
/// per id, so charging a document per reference would refuse ordinary exports,
/// which reference one full-canvas clip from every element they draw.
///
/// The `<use>` edge is left to `subtree_cost` for both halves of this: a cycle
/// through it is reported there as the expansion it is, and its chain depth is
/// charged against [`MAX_REFERENCE_HOPS`] there too, in the walk that already
/// has to follow it.
fn refuse_reference_cycles<'a, 'i>(
	root: roxmltree::Node<'a, 'i>,
	targets: &HashMap<&'a str, roxmltree::Node<'a, 'i>>,
	colour: &mut [u8],
) -> Result<(), ThumbError> {
	colour[root.id().get_usize()] = GREY;
	let mut stack = vec![(RefFrame::new(root), 0u32)];
	while let Some((frame, hops)) = stack.last_mut() {
		let hops = *hops;
		if let Some((dep, by_reference)) = frame.next_dep(targets) {
			let id = dep.id().get_usize();
			match colour[id] {
				GREY => {
					return Err(ThumbError::Decode(
						"svg: a recursive reference is refused — resolving it has no fixed point"
							.into(),
					));
				}
				BLACK => {}
				_ => {
					let hops = hops + u32::from(by_reference);
					if hops > MAX_REFERENCE_HOPS {
						return Err(ThumbError::Decode(
							"svg: references chained deeper than the resolver's stack survives \
							 are refused"
								.into(),
						));
					}
					colour[id] = GREY;
					stack.push((RefFrame::new(dep), hops));
				}
			}
		} else {
			let (done, _) = stack.pop().expect("the frame was there a line ago");
			colour[done.node.id().get_usize()] = BLACK;
		}
	}
	Ok(())
}

/// Whether an element carries a `filter`, as a presentation attribute or
/// inside its inline `style`.
fn references_filter(node: roxmltree::Node) -> bool {
	node.attributes().any(|attr| {
		attr.name() == "filter" || (attr.name() == "style" && attr.value().contains("filter"))
	})
}

/// Every `url(#id)` a stylesheet names. CSS is applied by usvg before anything
/// this pass can see, so the ids are all it can learn from one.
fn css_link_ids(text: &str) -> impl Iterator<Item = &str> {
	text.match_indices("url(")
		.filter_map(|(at, _)| func_iri(&text[at..]))
}

/// What one element's copies cost — markers and filter chains — and the
/// viewport a percentage length resolves against.
#[derive(Clone, Copy)]
struct NodeCharge {
	/// Nodes the most expensive `<marker>` subtree materialises — paid once per
	/// vertex of every shape, since a `<style>` block can put `marker-mid` on
	/// anything and no scan of the text can rule an element out.
	per_vertex: u64,
	/// Nodes the most expensive `<filter>` chain materialises, paid once by
	/// every element that can carry a filter.
	per_filter: u64,
	/// Whether a stylesheet names a `<filter>`, in which case every element
	/// can carry one and the attributes cannot rule any out.
	filter_anywhere: bool,
	viewport: f64,
}

impl NodeCharge {
	/// The nodes this element materialises on its own: itself, the marker
	/// copies its vertices take, and the filter chain copied onto it.
	///
	/// Charging those HERE, inside the memoised per-node cost, rather than
	/// once per source element afterwards, is what makes a `<use>` copy pay for
	/// them: usvg re-runs marker expansion and copies the resolved filter chain
	/// for every copy of an element, so both are a product with the `<use>`
	/// fan-out, not a sum beside it. `scan_document` counts an `fe*` tag once
	/// per physical occurrence, which 8000 `<use>`s of one 64-primitive filter
	/// — 130 KB — turned into a 164 MiB peak and a returned thumbnail; and the
	/// post-parse `layer_peak` walk runs downstream of the allocation it is
	/// meant to bound, so only this pass can catch it.
	fn node_cost(self, node: roxmltree::Node) -> u64 {
		let filter = if self.filter_anywhere || references_filter(node) {
			self.per_filter
		} else {
			0
		};
		(1u64)
			.saturating_add(marker_vertices(node, self.viewport).saturating_mul(self.per_vertex))
			.saturating_add(filter)
	}
}

/// Nodes the subtree at `root` materialises, memoised in `cost` and charged to
/// `spent`, and the longest `<use>` chain it contains, memoised in `hops`.
/// Iterative on purpose: a `<use>` chain is a dependency edge, not a tree
/// edge, so recursion here would be as deep as the chain — the very thing this
/// pre-pass exists to survive.
///
/// The hop count rides along because usvg's own `<use>` resolution IS that
/// recursion, and nothing else bounded it: `refuse_reference_cycles` skips the
/// `<use>` edge deliberately (it is an expansion, costed here), so the only
/// bound on a `<use>` chain used to be [`MAX_EXPANSION_NODES`] — a memory
/// budget doing stack duty by accident. Charging one costs ~n²/2 nodes, so
/// that budget let a 445-link chain through; 300 links were measured at
/// 1.1-1.25 MiB of the 2 MiB stack a `spawn_blocking` decode thread gets, and
/// ~180 abort outright in an unoptimised build.
fn subtree_cost<'a, 'i>(
	root: roxmltree::Node<'a, 'i>,
	targets: &HashMap<&'a str, roxmltree::Node<'a, 'i>>,
	charge: NodeCharge,
	cost: &mut [u64],
	hops: &mut [u32],
	colour: &mut [u8],
	spent: &mut u64,
) -> Result<u64, ThumbError> {
	let too_big = || {
		ThumbError::Decode(
			"svg: expansion past the node budget — <use> and marker copies would materialise \
			 more nodes than any budget here can hold"
				.into(),
		)
	};
	let too_deep = || {
		ThumbError::Decode(
			"svg: <use> chained deeper than the resolver's stack survives is refused".into(),
		)
	};
	if colour[root.id().get_usize()] == BLACK {
		return Ok(cost[root.id().get_usize()]);
	}
	colour[root.id().get_usize()] = GREY;
	*spent = spent.saturating_add(charge.node_cost(root));
	let mut stack = vec![Frame::new(
		root,
		use_target(root, targets),
		charge.node_cost(root),
		false,
	)];
	while let Some(frame) = stack.last_mut() {
		if let Some((dep, by_link)) = frame.next_dep() {
			let id = dep.id().get_usize();
			match colour[id] {
				GREY => {
					return Err(ThumbError::Decode(
						"svg: recursive <use> is refused — the expansion has no fixed point".into(),
					));
				}
				// Already costed, and being reached again: a `<use>` copy of
				// it, which materialises the whole subtree a second time.
				BLACK => {
					frame.cost = frame.cost.saturating_add(cost[id]);
					*spent = spent.saturating_add(cost[id]);
					frame.hops = frame.hops.max(hops[id].saturating_add(by_link.into()));
					if frame.hops > MAX_REFERENCE_HOPS {
						return Err(too_deep());
					}
				}
				_ => {
					// A `<use>` chain can outrun the element nesting the byte
					// scan bounded; cap the working stack at the same depth
					// usvg gives the whole parse.
					if stack.len() >= MAX_ELEMENT_DEPTH {
						return Err(too_big());
					}
					colour[id] = GREY;
					stack.push(Frame::new(
						dep,
						use_target(dep, targets),
						charge.node_cost(dep),
						by_link,
					));
					*spent = spent.saturating_add(charge.node_cost(dep));
				}
			}
		} else {
			let done = stack.pop().expect("the frame was there a line ago");
			colour[done.node.id().get_usize()] = BLACK;
			cost[done.node.id().get_usize()] = done.cost;
			hops[done.node.id().get_usize()] = done.hops;
			if let Some(parent) = stack.last_mut() {
				parent.cost = parent.cost.saturating_add(done.cost);
				parent.hops = parent
					.hops
					.max(done.hops.saturating_add(done.via_link.into()));
				if parent.hops > MAX_REFERENCE_HOPS {
					return Err(too_deep());
				}
			}
		}
		if *spent > MAX_EXPANSION_NODES {
			return Err(too_big());
		}
	}
	Ok(cost[root.id().get_usize()])
}

/// Conservative upper bound on the marker vertices one element carries.
///
/// EVERY shape usvg's `shapes::convert` accepts takes markers — not just the
/// four that spell their vertices out, but `rect`, `circle` and `ellipse`,
/// which it builds a path for. Missing those made a marker on a document full
/// of `<circle>` cost nothing at all.
///
/// For the spelled-out four, every vertex needs at least one number and every
/// number at least one run of digits, so counting digit runs cannot undercount
/// (decimals and exponents make it overcount, which costs an attacker and
/// nobody else). The `+ 2` covers `<line>`, whose two vertices live in four
/// separate attributes.
fn marker_vertices(node: roxmltree::Node, viewport: f64) -> u64 {
	fn digit_runs(s: &str) -> u64 {
		let mut runs = 0u64;
		let mut inside = false;
		for b in s.bytes() {
			if b.is_ascii_digit() && !inside {
				runs += 1;
			}
			inside = b.is_ascii_digit();
		}
		runs
	}
	let spelled_out = match node.tag_name().name() {
		"path" | "line" | "polyline" | "polygon" => (2u64)
			.saturating_add(node.attribute("d").map_or(0, digit_runs))
			.saturating_add(node.attribute("points").map_or(0, digit_runs)),
		"circle" | "ellipse" => 2,
		// Four sides and a close; the corner arcs are charged below.
		"rect" => 6,
		_ => return 0,
	};
	spelled_out.saturating_add(shape_arc_vertices(node, viewport))
}

/// Vertices the ARCS in one element flatten into, kept apart from the vertices
/// its markers copy onto.
///
/// Separate because the two are guarded differently: a detailed icon carrying
/// thousands of coordinates is perfectly ordinary, so `marker_vertices` may not
/// be capped — but an arc struck at an absurd radius is a bomb whatever it is
/// attached to, and only this half may be measured against
/// `MAX_SHAPE_ARC_VERTICES`.
fn shape_arc_vertices(node: roxmltree::Node, viewport: f64) -> u64 {
	// The corner radius, which is what the built shapes' arcs are struck at;
	// `rx`/`ry` stand in for each other, so the larger decides.
	let corner = |names: &[&str]| {
		names
			.iter()
			.filter_map(|name| node.attribute(*name))
			.map(|value| declared_length(value, viewport))
			.fold(0.0, f64::max)
	};
	match node.tag_name().name() {
		"circle" => arc_vertices(corner(&["r"])),
		// A rounded rect strikes the same arc at its corners.
		"ellipse" | "rect" => arc_vertices(corner(&["rx", "ry"])),
		// usvg hands `d`'s `A`/`a` commands to the SAME kurbo flattener the
		// built shapes go through, so an eleven-character arc buys exactly the
		// blow-up `<circle r="1e30"/>` does. Charging a path only its digit
		// runs missed it entirely.
		"path" => {
			let (arcs, largest) = path_arcs(node.attribute("d").unwrap_or(""));
			arcs.saturating_mul(arc_vertices(largest))
		}
		_ => 0,
	}
}

/// How many arc commands a path `d` carries, and the largest magnitude any
/// number in it reaches.
///
/// Only an arc's `rx`/`ry` decide how finely kurbo subdivides it, but taking
/// the maximum over the whole attribute is simpler and cannot undercount —
/// over-charging costs an attacker and nobody else.
///
/// Scanned character by character rather than split on separators, because SVG
/// lets numbers run together: `10-5` is two numbers and `1.2.3` is two more.
/// A minified icon is full of both, and reading either as one unparseable
/// token would refuse perfectly ordinary documents.
fn path_arcs(d: &str) -> (u64, f64) {
	let bytes = d.as_bytes();
	let (mut arcs, mut largest, mut i) = (0u64, 0.0f64, 0usize);
	while i < bytes.len() {
		match bytes[i] {
			b'A' | b'a' => {
				arcs = arcs.saturating_add(1);
				i += 1;
			}
			b if b.is_ascii_digit() || b == b'.' || b == b'+' || b == b'-' => {
				let start = i;
				if bytes[i] == b'+' || bytes[i] == b'-' {
					i += 1;
				}
				// A second `.` starts the next number, per SVG's grammar.
				let mut seen_dot = false;
				while i < bytes.len()
					&& (bytes[i].is_ascii_digit() || (bytes[i] == b'.' && !seen_dot))
				{
					seen_dot |= bytes[i] == b'.';
					i += 1;
				}
				// An exponent only counts if digits actually follow it.
				if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
					let exponent = i;
					i += 1;
					if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
						i += 1;
					}
					if i < bytes.len() && bytes[i].is_ascii_digit() {
						while i < bytes.len() && bytes[i].is_ascii_digit() {
							i += 1;
						}
					} else {
						i = exponent;
					}
				}
				if d[start..i].bytes().any(|b| b.is_ascii_digit()) {
					// A run that will not parse must not read as zero: charge
					// it the largest magnitude there is rather than undercount.
					let number = d[start..i].parse::<f64>().unwrap_or(f64::INFINITY).abs();
					largest = largest.max(number);
				} else {
					// A lone sign or dot: not a number, do not stall on it.
					i = start + 1;
				}
			}
			_ => i += 1,
		}
	}
	(arcs, largest)
}

/// Vertices the four quarter-arcs of a built shape flatten into.
///
/// Not a constant: usvg hands kurbo a 0.1 tolerance, and kurbo subdivides an
/// arc into `ceil(max(4, (11.163·r)^(1/6)) / 4)` cubics per quarter turn. That
/// is four for every radius under ~90 px and grows very slowly — but it does
/// grow, and `r="1e30"` is four characters: one such `<circle>` flattens into
/// millions of segments.
fn arc_vertices(radius: f64) -> u64 {
	// usvg skips a shape whose size is missing, zero or unparseable, so it has
	// no vertices to hang a marker on. (NaN lands here too, as it should.)
	if radius <= 0.0 || radius.is_nan() {
		return 0;
	}
	let per_quarter = ((11.163 * radius).powf(1.0 / 6.0).max(4.0) / 4.0).ceil();
	// Clamped far below `u64::MAX`, not at it: the count is only ever compared
	// against a budget, and `u64::MAX as f64` rounds to 2^64, so saturating
	// there and then adding to it wrapped back to a small number — which read
	// as "well under budget" and turned the guard below into a no-op.
	(4.0 * per_quarter).min(f64::from(u32::MAX)) as u64
}

/// A CSS length in user units, deliberately over-read: the number is taken
/// as written and any unit but `%` is charged the largest scale factor there
/// is (1in = 96px) instead of being parsed, since only the magnitude matters
/// and over-charging an attacker is free.
fn declared_length(value: &str, viewport: f64) -> f64 {
	let value = value.trim();
	// The unit is the trailing run of ASCII letters (or a `%`); summing char
	// widths keeps a multi-byte char from splitting the slice mid-character.
	let unit_len: usize = value
		.chars()
		.rev()
		.take_while(|c| c.is_ascii_alphabetic() || *c == '%')
		.map(char::len_utf8)
		.sum();
	let unit_start = value.len() - unit_len;
	let Ok(number) = value[..unit_start].parse::<f64>() else {
		return 0.0;
	};
	if !number.is_finite() {
		return 0.0;
	}
	if value[unit_start..].starts_with('%') {
		number.abs() / 100.0 * viewport
	} else {
		number.abs() * 96.0
	}
}

/// What a percentage length resolves against: the largest viewport ANY `<svg>`
/// in the document declares, or the SVG default when none does.
///
/// Not the root's: usvg swaps `state.view_box` for each nested `<svg>`'s own
/// viewBox (`use_node::convert_svg`, reached for every non-root `<svg>`), and
/// `units::convert_length` resolves a percentage `rx`/`ry` against THAT. So
/// `<svg><svg viewBox="0 0 1e36 1e36"><ellipse rx="50%" ry="50%"/></svg></svg>`
/// — 120 bytes — walked straight past `MAX_SHAPE_ARC_VERTICES` and rendered at
/// a 66 MiB peak, while the identical viewBox on the ROOT was refused in 20 µs.
///
/// Charging every percentage in the document the largest viewport in it
/// over-reads a document that nests a big viewport beside a small one; which
/// nested viewport an element really sits in costs a second tree walk, and
/// over-charging is free here.
fn max_viewport(doc: &roxmltree::Document) -> f64 {
	doc.descendants()
		.filter(|node| node.is_element() && node.tag_name().name() == "svg")
		.map(declared_viewport)
		.fold(100.0, f64::max)
}

/// The viewport one `<svg>` element declares, or 0 when it declares none.
fn declared_viewport(node: roxmltree::Node) -> f64 {
	let side = |name| {
		node.attribute(name)
			.map_or(0.0, |v| declared_length(v, 100.0))
	};
	let view_box = node.attribute("viewBox").map_or(0.0, |v| {
		v.split([' ', '\t', '\n', '\r', ','])
			.filter_map(|p| p.parse::<f64>().ok())
			.filter(|n| n.is_finite())
			.skip(2)
			.fold(0.0, |acc: f64, n| acc.max(n.abs()))
	});
	side("width").max(side("height")).max(view_box)
}

/// Upper bound on the nodes `usvg::Tree::from_str` will materialise, or an
/// error when the document is past [`MAX_EXPANSION_NODES`].
///
/// This is the only place a bound can be applied. usvg resolves `<use>` by
/// deep-copying the target subtree and copies a `<marker>` subtree once per
/// path vertex, both DURING the parse, and its only backstop (a 1M-node limit)
/// fires after hundreds of megabytes are already allocated. Costs are memoised
/// per element, so a doubling chain that expands to 2^24 nodes is still walked
/// in time linear in the text.
///
/// Markers are charged bluntly: every shape vertex in the document pays for the
/// most expensive marker declared, because which paths carry markers cannot be
/// known without resolving CSS (a `<style>` block can set `marker-mid` on
/// everything). Arrow diagrams are a few dozen vertices and pass regardless.
fn expansion_cost(text: &str) -> Result<usize, ThumbError> {
	// Same parser and options usvg will use, so a document that survives this
	// pass is one it can also read.
	let options = roxmltree::ParsingOptions {
		allow_dtd: true,
		..Default::default()
	};
	let doc = roxmltree::Document::parse_with_options(text, options)
		.map_err(|e| ThumbError::Decode(format!("svg: {e}")))?;

	let viewport = max_viewport(&doc);
	let mut nodes = 0usize;
	let mut targets: HashMap<&str, roxmltree::Node> = HashMap::new();
	let mut markers = Vec::new();
	let mut filters = Vec::new();
	let mut styled_markers = false;
	let mut css_links = Vec::new();
	for node in doc.descendants() {
		nodes = nodes.max(node.id().get_usize() + 1);
		if !node.is_element() {
			continue;
		}
		if let Some(id) = node.attribute("id") {
			// usvg's own rule: the first declaration of an id wins.
			targets.entry(id).or_insert(node);
		}
		match node.tag_name().name() {
			"marker" => markers.push(node),
			"filter" => filters.push(node),
			// CSS can put a marker on anything, this pass included — a
			// stylesheet that mentions one is treated as if it did. The
			// `url(#…)`s a stylesheet names are all this pass can learn about
			// the rest of what CSS applies: matching the selectors is usvg's
			// job, and it does it before anything here can see the result.
			"style" => {
				for text in node.children().filter_map(|child| child.text()) {
					styled_markers |= text.contains("marker");
					css_links.extend(css_link_ids(text));
				}
			}
			// A radius big enough to flatten into this many segments is a
			// bomb on its own, marker or no marker: kurbo subdivides by the
			// radius, and 300 bytes of `<circle r="1e30"/>` peaked at 23 MB.
			// `<path>` counts too — the same arc spelled `A1e30,1e30 …` ran
			// 9.6 s and then panicked inside tiny-skia on a non-finite point.
			"circle" | "ellipse" | "rect" | "path"
				if shape_arc_vertices(node, viewport) > MAX_SHAPE_ARC_VERTICES =>
			{
				return Err(ThumbError::Decode(
					"svg: a shape radius that flattens into more path segments than the budget \
					 can hold is refused"
						.into(),
				));
			}
			_ => {}
		}
	}

	// A mask or clip a STYLESHEET names can land on any element, so the walk
	// below cannot see the edge and the recursion it feeds is the one that
	// aborts the process: the same three-`<mask>` cycle written as CSS killed
	// the decode exactly as the attribute spelling did. Refusing is what the
	// marker path already does with a stylesheet it cannot resolve; a `<style>`
	// naming a gradient — by far the common case — is untouched, gradients
	// having no `mask`/`clip-path` to recurse through.
	let mut filter_anywhere = false;
	for id in css_links {
		match targets.get(id).map(|node| node.tag_name().name()) {
			Some("mask" | "clipPath") => {
				return Err(ThumbError::Decode(
					"svg: a stylesheet that names a <mask> or <clipPath> is refused — CSS \
					 can put one on any element and usvg resolves them recursively"
						.into(),
				));
			}
			Some("filter") => filter_anywhere = true,
			_ => {}
		}
	}

	let charge = NodeCharge {
		per_vertex: marker_cost(&markers, styled_markers)?,
		per_filter: filter_cost(&filters),
		filter_anywhere,
		viewport,
	};
	let mut cost = vec![0u64; nodes];
	let mut hops = vec![0u32; nodes];
	let mut colour = vec![WHITE; nodes];
	refuse_reference_cycles(doc.root_element(), &targets, &mut colour)?;
	colour.fill(WHITE);
	let mut spent = 0u64;
	subtree_cost(
		doc.root_element(),
		&targets,
		charge,
		&mut cost,
		&mut hops,
		&mut colour,
		&mut spent,
	)?;
	Ok(usize::try_from(spent).unwrap_or(usize::MAX))
}

/// Nodes the most expensive `<marker>` materialises per vertex it is copied to.
///
/// Markers are the one expansion that has no fixed point available: usvg only
/// blocks a marker that is its OWN ancestor (`parser/marker.rs`), so a marker
/// whose path carries a second marker is copied once per vertex per vertex —
/// two 450-vertex paths peaked at 169 MB from 4 KB of document. Costing that
/// means solving for the product over every chain the document (and its CSS)
/// permits; refusing it means one walk. Nothing real puts a marker on a marker,
/// so the walk is what this does — and with no chain possible, a marker subtree
/// materialises exactly the elements it spells out.
fn marker_cost(markers: &[roxmltree::Node], styled_markers: bool) -> Result<u64, ThumbError> {
	if markers.is_empty() {
		return Ok(0);
	}
	let chained = || {
		ThumbError::Decode(
			"svg: a <marker> that can itself carry a marker is refused — the copies multiply \
			 with no fixed point"
				.into(),
		)
	};
	if styled_markers {
		return Err(chained());
	}
	let mut worst = 0u64;
	for marker in markers {
		// Marker attributes inherit, so an ancestor's counts too — and a
		// `<use>` inside a marker could pull one in from anywhere in the
		// document, which is likewise not an idiom worth resolving.
		if marker.ancestors().skip(1).any(references_marker) {
			return Err(chained());
		}
		let mut count = 0u64;
		for node in marker.descendants().filter(roxmltree::Node::is_element) {
			if references_marker(node) || node.tag_name().name() == "use" {
				return Err(chained());
			}
			count += 1;
		}
		worst = worst.max(count);
	}
	Ok(worst)
}

/// Nodes the most expensive `<filter>` materialises per element it lands on.
///
/// Bounded by `MAX_FILTER_PRIMITIVE_TAGS`, which already caps the `fe*` tags a
/// document may carry at all — so this is at most a filter and 64 primitives,
/// weighted by what one copied primitive really costs.
fn filter_cost(filters: &[roxmltree::Node]) -> u64 {
	filters
		.iter()
		.map(|filter| {
			filter
				.descendants()
				.filter(|node| node.is_element())
				.count() as u64
		})
		.max()
		.unwrap_or(0)
		.saturating_mul(FILTER_NODE_WEIGHT)
}

/// Whether an element names a marker, as a presentation attribute or inside its
/// inline `style`.
fn references_marker(node: roxmltree::Node) -> bool {
	node.attributes().any(|attr| {
		matches!(
			attr.name(),
			"marker" | "marker-start" | "marker-mid" | "marker-end"
		) || (attr.name() == "style" && attr.value().contains("marker"))
	})
}

/// Aspect ratio (w/h) from the root tag: absolute `width`/`height` first,
/// else the `viewBox`, else square. Percentages and font-relative units carry
/// no absolute size and fall through.
fn root_aspect(bytes: &[u8], root: usize) -> f64 {
	let mut width = None;
	let mut height = None;
	let mut view_box = None;
	for (name, value) in RootAttrs::new(bytes, root) {
		match name {
			b"width" => width = absolute_length(value),
			b"height" => height = absolute_length(value),
			b"viewBox" => view_box = view_box_aspect(value),
			_ => {}
		}
	}
	match (width, height) {
		(Some(w), Some(h)) => w / h,
		_ => view_box.unwrap_or(1.0),
	}
}

/// Attribute iterator over one start tag; quotes are honoured so a `>` inside
/// a value does not end the tag.
struct RootAttrs<'a> {
	bytes: &'a [u8],
	i: usize,
}

impl<'a> RootAttrs<'a> {
	fn new(bytes: &'a [u8], root: usize) -> Self {
		let mut i = root + 1;
		while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'>' | b'/')
		{
			i += 1;
		}
		RootAttrs { bytes, i }
	}
}

impl<'a> Iterator for RootAttrs<'a> {
	type Item = (&'a [u8], &'a str);

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			while self.bytes.get(self.i)?.is_ascii_whitespace() {
				self.i += 1;
			}
			if matches!(self.bytes.get(self.i)?, b'>' | b'/') {
				return None;
			}
			let name_start = self.i;
			while !matches!(self.bytes.get(self.i)?, b'=' | b'>' | b'/')
				&& !self.bytes[self.i].is_ascii_whitespace()
			{
				self.i += 1;
			}
			let name = &self.bytes[name_start..self.i];
			while self.bytes.get(self.i)?.is_ascii_whitespace() {
				self.i += 1;
			}
			if self.bytes.get(self.i) != Some(&b'=') {
				continue;
			}
			self.i += 1;
			while self.bytes.get(self.i)?.is_ascii_whitespace() {
				self.i += 1;
			}
			let quote = *self.bytes.get(self.i)?;
			if !matches!(quote, b'"' | b'\'') {
				// Unquoted values are not XML; skip the junk token.
				while !self.bytes.get(self.i)?.is_ascii_whitespace() && self.bytes[self.i] != b'>' {
					self.i += 1;
				}
				continue;
			}
			self.i += 1;
			let value_start = self.i;
			while *self.bytes.get(self.i)? != quote {
				self.i += 1;
			}
			let value = &self.bytes[value_start..self.i];
			self.i += 1;
			if let Ok(value) = std::str::from_utf8(value) {
				return Some((name, value));
			}
		}
	}
}

/// A positive, finite CSS length in absolute units, normalised to px. Only
/// the ratio matters here, but mixed units (`width="10cm" height="200px"`)
/// still need the conversion to keep it right.
fn absolute_length(value: &str) -> Option<f64> {
	let value = value.trim();
	// The unit is the trailing run of ASCII letters (or a `%`). Split it off by
	// summing those chars' widths, never by byte arithmetic on `rfind`: that
	// hands back the START of the last non-unit char, and `+ 1` on a multi-byte
	// one (`width="1é"`) lands mid-char and panics the slice.
	let unit_len: usize = value
		.chars()
		.rev()
		.take_while(|c| c.is_ascii_alphabetic() || *c == '%')
		.map(char::len_utf8)
		.sum();
	let unit_start = value.len() - unit_len;
	let scale = match &value[unit_start..] {
		"" | "px" => 1.0,
		"in" => 96.0,
		"cm" => 96.0 / 2.54,
		"mm" => 96.0 / 25.4,
		"pt" => 96.0 / 72.0,
		"pc" => 16.0,
		_ => return None,
	};
	let number: f64 = value[..unit_start].parse().ok()?;
	(number.is_finite() && number > 0.0).then_some(number * scale)
}

fn view_box_aspect(value: &str) -> Option<f64> {
	let mut parts = value
		.split([' ', '\t', '\n', '\r', ','])
		.filter(|p| !p.is_empty());
	let _x: f64 = parts.next()?.parse().ok()?;
	let _y: f64 = parts.next()?.parse().ok()?;
	let w: f64 = parts.next()?.parse().ok()?;
	let h: f64 = parts.next()?.parse().ok()?;
	(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then_some(w / h)
}

/// The raster this decode commits to: the document aspect covering the
/// (already budget-clamped) target, long side capped so a degenerate aspect
/// cannot buy a mile-wide pixmap.
fn raster_dims(aspect: f64, spec: &ThumbSpec) -> (u32, u32) {
	let aspect = if aspect.is_finite() && aspect > 0.0 {
		aspect
	} else {
		1.0
	};
	let (tw, th) = (
		f64::from(spec.target_width.max(1)),
		f64::from(spec.target_height.max(1)),
	);
	let (mut w, mut h) = if aspect >= tw / th {
		(th * aspect, th)
	} else {
		(tw, tw / aspect)
	};
	let long = w.max(h);
	let cap = f64::from(crate::MAX_CANVAS_LONG_SIDE);
	if long > cap {
		w = w * cap / long;
		h = h * cap / long;
	}
	// The pixmap is not the only pixmap: every isolated layer allocates
	// another one of roughly its size, and `LAYERED_ALLOWANCE_PIXMAPS` of them
	// are priced against the same budget. Cap the area at what the raster plus
	// that allowance may take of it — a wide document renders smaller rather
	// than not at all.
	//
	// The reserve is counted in UNPADDED pixels while the allowance charges
	// `LAYER_PAD` on each side; that gap is what the ×2 here is spent on. It
	// only ever binds above ~470 px a side (below that nothing shrinks), where
	// the padding is under 2% — so the reserve still covers the estimate with
	// most of the doubling to spare.
	let max_px = (spec.mem_budget / (4 * 2 * (1 + LAYERED_ALLOWANCE_PIXMAPS))).max(1) as f64;
	if w * h > max_px {
		let shrink = (max_px / (w * h)).sqrt();
		w *= shrink;
		h *= shrink;
	}
	((w.round() as u32).max(1), (h.round() as u32).max(1))
}

struct PreparedSvg {
	text: String,
	out_dims: (u32, u32),
	tag_count: usize,
	attr_count: usize,
	/// What the expansion pre-pass costed the document at, or 0 when it was
	/// skipped because the cheap estimate had already blown the budget.
	effective_nodes: usize,
}

impl PreparedSvg {
	fn layer_allowance(&self) -> usize {
		layer_unit_bytes(self.out_dims).saturating_mul(LAYERED_ALLOWANCE_PIXMAPS)
	}

	/// What one isolated layer can cost at most, matching the clamp resvg
	/// itself applies: every layer's bounding box is fitted to `max_bbox`,
	/// 5×5 canvases, before its sub-pixmap is allocated.
	fn layer_cap(&self) -> u64 {
		pixmap_bytes(self.out_dims).saturating_mul(25) as u64
	}
}

fn pixmap_bytes(dims: (u32, u32)) -> usize {
	dims.0 as usize * dims.1 as usize * 4
}

/// Bytes ONE isolated layer covering the whole canvas costs, in the units
/// `layer_peak` charges in — the output pixmap plus the padding resvg rounds
/// every layer out by. The allowance is quoted in these so a verdict depends
/// on the document, not on how big the thumbnail happens to be.
fn layer_unit_bytes(dims: (u32, u32)) -> usize {
	pixmap_bytes((
		dims.0.saturating_add(LAYER_PAD),
		dims.1.saturating_add(LAYER_PAD),
	))
}

impl PreparedDecode for PreparedSvg {
	fn dims(&self) -> (u32, u32) {
		// A vector source has no pixel dimensions; the committed raster is
		// the only honest answer, and it keeps the orchestrator's source-area
		// ceilings from tripping on a meaningless declared size.
		self.out_dims
	}

	fn embedded_preview(&mut self, _mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		// No such thing for SVG.
		Ok(None)
	}

	fn peak_estimate(&self) -> usize {
		// Held text ×3 covers the text plus usvg's parsed path data and
		// per-node strings; the per-tag/per-attr terms cover the roxmltree
		// and usvg svgtree nodes (~120 B + ~64 B a node, ~70 B an attribute,
		// rounded up); the pixmap term carries the render target plus the
		// isolated-layer allowance `decode_into` enforces before rendering.
		//
		// Nodes are the larger of what the text holds and what the expansion
		// pre-pass costed it at: `<use>` and markers materialise nodes the
		// text never spells out, and those cost the same per node as the rest.
		self.text
			.len()
			.saturating_mul(3)
			.saturating_add(self.tag_count.max(self.effective_nodes).saturating_mul(256))
			.saturating_add(self.attr_count.saturating_mul(128))
			.saturating_add(pixmap_bytes(self.out_dims))
			.saturating_add(self.layer_allowance())
			.saturating_add(self.out_dims.0 as usize * 4)
	}

	fn decode_into(self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let options = usvg::Options {
			// The defaults resolve `<image>` hrefs — the string resolver
			// reads local FILES. An untrusted document gets neither that nor
			// embedded rasters: both resolvers answer None and the elements
			// silently drop. usvg makes no network requests on its own.
			image_href_resolver: usvg::ImageHrefResolver {
				resolve_data: Box::new(|_, _, _| None),
				resolve_string: Box::new(|_, _| None),
			},
			..usvg::Options::default()
		};
		let tree = usvg::Tree::from_str(&self.text, &options)
			.map_err(|e| ThumbError::Decode(format!("svg: {e}")))?;
		let (ow, oh) = self.out_dims;
		let size = tree.size();
		let scale =
			(f64::from(ow) / f64::from(size.width())).min(f64::from(oh) / f64::from(size.height()));
		// The render transform is f32, and a subnormal document size fits an
		// f64 scale that overflows f32 to infinity, which renders nothing.
		if !(scale > 0.0 && scale <= f64::from(f32::MAX)) {
			return Err(ThumbError::Decode("svg: degenerate document size".into()));
		}
		// The estimate promised a fixed render allowance; hold the real tree to
		// it before anything allocates. Dashing happens inside the stroke of a
		// path that is itself inside the layer stack, so the two are concurrent
		// and share the allowance.
		let allowance = self.layer_allowance() as u64;
		let dashes = dash_peak(tree.root());
		if dashes > allowance {
			return Err(ThumbError::Decode(
				"svg: stroke-dasharray expands past the memory allowance".into(),
			));
		}
		if layer_peak(tree.root(), scale, self.layer_cap()).saturating_add(dashes) > allowance {
			return Err(ThumbError::Decode(
				"svg: isolated layer stack exceeds the memory allowance".into(),
			));
		}
		let Some(mut pixmap) = tiny_skia::Pixmap::new(ow, oh) else {
			return Err(ThumbError::Decode("svg: invalid raster dimensions".into()));
		};
		// Uniform fit, centered: the aspect from `open` and usvg's can differ
		// on exotic documents, and a letterboxed render beats a distorted one.
		let scale = scale as f32;
		let ts = tiny_skia::Transform::from_scale(scale, scale).post_translate(
			(ow as f32 - size.width() * scale) / 2.0,
			(oh as f32 - size.height() * scale) / 2.0,
		);
		resvg::render(&tree, ts, &mut pixmap.as_mut());
		let mut row = vec![0u8; ow as usize * 4];
		let pixels = pixmap.pixels();
		for y in 0..oh {
			let src = &pixels[y as usize * ow as usize..][..ow as usize];
			for (dst, px) in row.chunks_exact_mut(4).zip(src) {
				let px = px.demultiply();
				dst.copy_from_slice(&[px.red(), px.green(), px.blue(), px.alpha()]);
			}
			sink.push(0, y, ow, &row)?;
		}
		Ok(())
	}
}

/// Bytes one dash segment costs at its peak: two verbs and two points in the
/// dashed path tiny-skia builds (~18 B), plus the stroke outline it then
/// builds from that — a cap or join per segment end, ~16 points. Measured at
/// tiny-skia's own ceiling of a million dashes per contour, which peaks around
/// 125 MB.
const DASH_BYTES: f64 = 144.0;

/// Worst-case bytes a dashed stroke will hold while this tree renders.
///
/// tiny-skia caps dashing at a million segments per path — but it BUILDS up to
/// that many, and a ~200-byte document reaches it: a long path with a short
/// dash interval peaked at 125 MB and still returned a thumbnail. One path is
/// dashed at a time (the dashed copy dies with the `stroke_path` call), so the
/// peak is the worst path, not the sum.
///
/// The count is tiny-skia's own: contour length × dash pairs ÷ interval. Note
/// that dashing runs on the path in its LOCAL coordinates — `stroke_path`
/// dashes before applying the transform — so no scale factor belongs here.
fn dash_peak(group: &usvg::Group) -> u64 {
	let mut worst = 0u64;
	for node in group.children() {
		worst = worst.max(match node {
			usvg::Node::Group(child) => {
				let masked = child.mask().map_or(0, |m| dash_peak(m.root()));
				dash_peak(child).max(masked)
			}
			usvg::Node::Path(path) => path_dash_bytes(path),
			// Text is never present (no font database) and images never
			// resolve; neither can carry a stroke here.
			_ => 0,
		});
	}
	worst
}

fn path_dash_bytes(path: &usvg::Path) -> u64 {
	let Some(dashes) = path.stroke().and_then(usvg::Stroke::dasharray) else {
		return 0;
	};
	let interval = f64::from(dashes.iter().sum::<f32>());
	if !(interval.is_finite() && interval > 0.0) {
		return 0;
	}
	// The control polygon is at least as long as the curve it hulls, so this
	// bounds the arc length without flattening anything.
	let mut length = 0.0;
	let mut cursor = tiny_skia::Point::zero();
	let mut contour_start = cursor;
	let mut step = |to: tiny_skia::Point, cursor: &mut tiny_skia::Point| {
		length += f64::from(cursor.distance(to));
		*cursor = to;
	};
	for segment in path.data().segments() {
		match segment {
			tiny_skia::PathSegment::MoveTo(p) => {
				cursor = p;
				contour_start = p;
			}
			tiny_skia::PathSegment::LineTo(p) => step(p, &mut cursor),
			tiny_skia::PathSegment::QuadTo(a, b) => {
				step(a, &mut cursor);
				step(b, &mut cursor);
			}
			tiny_skia::PathSegment::CubicTo(a, b, c) => {
				step(a, &mut cursor);
				step(b, &mut cursor);
				step(c, &mut cursor);
			}
			tiny_skia::PathSegment::Close => step(contour_start, &mut cursor),
		}
	}
	let pairs = (dashes.len() / 2) as f64;
	(length * pairs / interval * DASH_BYTES).min(u64::MAX as f64) as u64
}

/// Worst-case concurrent isolated-layer bytes resvg can hold while rendering
/// this tree at `scale`: every `should_isolate` group gets a bbox-sized
/// sub-pixmap, alive while its children (and their layers) render into it;
/// clip and mask each add pixmaps of the same size (×2 covers their nested
/// variants), and a filter chain keeps input, result and one buffer per
/// primitive. Images and text cannot appear (resolvers refuse, feature off).
///
/// `cap` is what resvg will really allocate for one layer however big its
/// bounding box is: it fits every layer to `max_bbox` (5×5 canvases) first.
/// Without that clamp an off-canvas bounding box — a shape at x=1e9 — made
/// this estimate diverge without limit from the allocation it is estimating,
/// and refused documents resvg would have rendered in a couple of pixmaps.
fn layer_peak(group: &usvg::Group, scale: f64, cap: u64) -> u64 {
	let mut deepest_child = 0u64;
	for node in group.children() {
		if let usvg::Node::Group(child) = node {
			deepest_child = deepest_child.max(layer_peak(child, scale, cap));
		}
	}
	let mut own = 0u64;
	if group.should_isolate() {
		let bbox = group.abs_layer_bounding_box();
		let w = (f64::from(bbox.width()) * scale).ceil() + f64::from(LAYER_PAD);
		let h = (f64::from(bbox.height()) * scale).ceil() + f64::from(LAYER_PAD);
		let bytes = if w.is_finite() && h.is_finite() {
			(w * h * 4.0).min(u64::MAX as f64) as u64
		} else {
			u64::MAX
		}
		.min(cap);
		own = bytes;
		if group.clip_path().is_some() {
			own = own.saturating_add(bytes.saturating_mul(2));
		}
		if let Some(mask) = group.mask() {
			own = own
				.saturating_add(bytes.saturating_mul(2))
				.saturating_add(layer_peak(mask.root(), scale, cap));
		}
		let primitives: usize = group.filters().iter().map(|f| f.primitives().len()).sum();
		if primitives > 0 {
			own = own.saturating_add(bytes.saturating_mul(2 + primitives as u64));
		}
	}
	own.saturating_add(deepest_child)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The pure guards all take text, so every case here is a string literal —
	/// no fixture, no encoder, and no reason for these to live in `tests/`,
	/// which `cargo test --lib` (what CI and the pre-push hook run) compiles
	/// but never executes.
	fn svg(body: &str) -> String {
		format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 10 10\">{body}</svg>")
	}

	fn refusal<T>(result: Result<T, ThumbError>) -> String {
		match result {
			Ok(_) => panic!("expected a refusal"),
			Err(e) => e.to_string(),
		}
	}

	/// A `<use>` chain `n` links long over one `<rect>`.
	///
	/// `base_first` emits the links in resolution order, so each one finds its
	/// target already costed and memoised: that arrangement keeps the walk's
	/// stack two frames deep and charges ~n²/2 nodes. Reversed, the walk
	/// descends the chain instead and charges ~n — which is why the node
	/// budget alone bounded the two arrangements at wildly different lengths.
	fn use_chain(n: usize, base_first: bool) -> String {
		let mut links: Vec<String> = (1..=n)
			.map(|i| format!("<use id=\"u{i}\" href=\"#u{}\"/>", i - 1))
			.collect();
		if !base_first {
			links.reverse();
		}
		svg(&format!(
			"<rect id=\"u0\" width=\"1\" height=\"1\"/>{}",
			links.concat()
		))
	}

	#[test]
	fn scan_counts_tags_and_attrs() {
		let scan = scan_document(b"<svg width=\"1\" height=\"2\"><g><rect x=\"0\"/></g></svg>")
			.expect("a plain document");
		assert_eq!(scan.tags, 5);
		assert_eq!(scan.attrs, 3);
	}

	#[test]
	fn scan_refuses_entity_declarations() {
		let doc = b"<!DOCTYPE svg [<!ENTITY a \"bbbb\">]><svg>&a;</svg>";
		assert!(refusal(scan_document(doc)).contains("entity declarations"));
	}

	#[test]
	fn scan_refuses_pattern_behind_a_namespace_prefix() {
		let doc = b"<svg><averylongnamespaceprefixindeed:pattern/></svg>";
		assert!(refusal(scan_document(doc)).contains("<pattern>"));
	}

	#[test]
	fn scan_refuses_too_many_filter_primitives() {
		let ok = format!("<svg><filter>{}</filter></svg>", "<feBlend/>".repeat(64));
		scan_document(ok.as_bytes()).expect("64 primitives is the cap, not past it");
		let over = format!("<svg><filter>{}</filter></svg>", "<feBlend/>".repeat(65));
		assert!(refusal(scan_document(over.as_bytes())).contains("filter primitives"));
	}

	#[test]
	fn scan_refuses_nesting_past_the_cap() {
		let over = format!(
			"{}{}",
			"<g>".repeat(MAX_ELEMENT_DEPTH + 1),
			"</g>".repeat(MAX_ELEMENT_DEPTH + 1)
		);
		assert!(refusal(scan_document(over.as_bytes())).contains("element nesting"));
	}

	#[test]
	fn expansion_charges_every_use_copy() {
		let plain = expansion_cost(&svg("<g id=\"a\"><rect/><rect/></g>")).expect("plain document");
		let copied = expansion_cost(&svg(
			"<g id=\"a\"><rect/><rect/></g><use href=\"#a\"/><use href=\"#a\"/>",
		))
		.expect("two copies");
		// Each `<use>` is a node of its own plus a second materialisation of
		// the three-node subtree it points at.
		assert_eq!(copied, plain + 2 * (1 + 3));
	}

	#[test]
	fn expansion_refuses_a_doubling_fan_out() {
		let mut body = String::from("<g id=\"g0\"><rect/></g>");
		for level in 1..20 {
			body.push_str(&format!(
				"<g id=\"g{level}\"><use href=\"#g{}\"/><use href=\"#g{}\"/></g>",
				level - 1,
				level - 1
			));
		}
		assert!(refusal(expansion_cost(&svg(&body))).contains("node budget"));
	}

	#[test]
	fn expansion_refuses_a_use_cycle() {
		let doc = svg("<g id=\"a\"><use href=\"#b\"/></g><g id=\"b\"><use href=\"#a\"/></g>");
		assert!(refusal(expansion_cost(&doc)).contains("recursive <use>"));
	}

	#[test]
	fn expansion_refuses_a_mask_cycle() {
		let doc = svg("<mask id=\"m1\"><rect mask=\"url(#m2)\"/></mask>\
			 <mask id=\"m2\"><rect mask=\"url(#m3)\"/></mask>\
			 <mask id=\"m3\"><rect mask=\"url(#m1)\"/></mask>");
		assert!(refusal(expansion_cost(&doc)).contains("recursive reference"));
	}

	/// The acyclic-but-deep half of the same defence: [`MAX_REFERENCE_HOPS`]
	/// on a `<mask>` chain, which is one usvg recursion level per link.
	#[test]
	fn expansion_refuses_a_long_mask_chain() {
		let chain = |n: usize| {
			let masks: String = (1..=n)
				.map(|i| {
					if i == n {
						format!("<mask id=\"m{i}\"><rect/></mask>")
					} else {
						format!("<mask id=\"m{i}\"><rect mask=\"url(#m{})\"/></mask>", i + 1)
					}
				})
				.collect();
			svg(&masks)
		};
		expansion_cost(&chain(MAX_REFERENCE_HOPS as usize + 1)).expect("a chain at the cap");
		assert!(
			refusal(expansion_cost(&chain(MAX_REFERENCE_HOPS as usize + 2)))
				.contains("chained deeper")
		);
	}

	/// Regression: a `<use>` chain is a usvg recursion level per link exactly
	/// as a `<mask>` chain is, but it used to be bounded only by
	/// [`MAX_EXPANSION_NODES`] — a memory budget standing in for a stack one.
	/// Both arrangements below cost far too little to trip that budget.
	#[test]
	fn expansion_refuses_a_long_use_chain() {
		for base_first in [true, false] {
			expansion_cost(&use_chain(MAX_REFERENCE_HOPS as usize, base_first))
				.expect("a chain at the cap");
			let over = use_chain(MAX_REFERENCE_HOPS as usize + 1, base_first);
			assert!(
				refusal(expansion_cost(&over)).contains("chained deeper"),
				"base_first={base_first}"
			);
		}
	}
}
