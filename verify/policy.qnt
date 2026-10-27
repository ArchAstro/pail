// -*- mode: Bluespec; -*-
/// Branch policy (`src/policy.rs`): reading `policy.json`, deciding whether a caller may do
/// an op to an object, and which op a request is. Every enforcement point implements it
/// (`pail serve`; the ArchDev gateway's Go port) and is checked against `policy_scenarios.qnt`
/// (`tests/policy_refinement.rs`, and the gateway's copy of the generated Go adapter).
///
/// Quint strings cannot be sliced, but keys are decided by their prefixes and branch globs
/// match by prefix, so keys and globs are `Text`: a string as its list of characters.
/// Identities are only compared, so they stay `str`.
module policy {
  // ---------- policy.json, as far as parsing it is concerned ----------

  /// A string as its characters.
  type Text = List[str]

  /// A field of a branch rule.
  type RuleField =
    | RNull
    | RInt(int)
    | RText(str)
    | RTexts(List[str])
    // Strings, then a null element.
    | RTextsThenNull(List[str])

  /// A branch rule's JSON value.
  type RuleDoc = RuleNull | RuleObject(str -> RuleField)

  /// A top-level field of `policy.json`.
  type Field =
    | FNull
    | FInt(int)
    | FText(str)
    | FTexts(List[str])
    // Strings, then a null element.
    | FTextsThenNull(List[str])
    | FRules(Text -> RuleDoc)

  /// `policy.json`: a JSON object, by field name (names are case-sensitive; unknown ones
  /// are ignored).
  type Doc = str -> Field

  // ---------- a parsed policy ----------

  type Approvals = NoApprovals | NeedsApprovals(int)

  type Rule = { writers: List[str], approvals: Approvals, requiredChecks: List[str] }

  type Policy = {
    readers: List[str],
    admins: List[str],
    gc: List[str],
    branches: Text -> Rule,
  }

  type Loaded = NoPolicy | Invalid | Valid(Policy)

  type Op = Read | Write | Delete

  /// Who is asking: a principal and the groups or roles it holds.
  type Caller = { principal: str, groups: Set[str] }

  /// The last answer an action gave.
  type Answer = NoAnswer | Allowed | Denied | Mapped(Op)

  type State = { policy: Loaded, answer: Answer }

  pure val U32_MAX: int = 4294967295

  // ---------- text ----------
  //
  // Substrings are spans `[start, end)` of a text, so no text is ever cut.

  /// Whether `p` occurs in `t` at `at`.
  pure def occursAt(t: Text, at: int, p: Text): bool =
    t.length() >= at + p.length() and p.indices().forall(i => t[at + i] == p[i])

  pure def startsWith(t: Text, p: Text): bool = occursAt(t, 0, p)

  /// The largest of `s`, or `none` if it is empty.
  pure def largest(s: Set[int], none: int): int =
    s.fold(none, (a, i) => if (a == none or i > a) i else a)

  pure val REFS: Text = ["r", "e", "f", "s", "/"]
  pure val INTENTS: Text = ["i", "n", "t", "e", "n", "t", "s", "/"]
  pure val REFLOG: Text = ["r", "e", "f", "l", "o", "g", "/"]
  pure val LOCKS: Text = ["l", "o", "c", "k", "s", "/"]
  pure val SHARED: Text = ["s", "h", "a", "r", "e", "d", "-"]
  pure val EXCLUSIVE: Text = ["e", "x", "c", "l", "u", "s", "i", "v", "e", "-"]
  pure val HEAD: Text = ["H", "E", "A", "D"]
  pure val CONTENT: Set[Text] = Set(
    ["c", "o", "m", "m", "i", "t", "s", "/"],
    ["p", "a", "g", "e", "s", "/"],
    ["p", "a", "c", "k", "s", "/"],
    ["c", "h", "u", "n", "k", "s", "/"],
  )

  pure def isContent(key: Text): bool = CONTENT.exists(p => startsWith(key, p))

  /// Where the last `/` at or after `from` is in `t`, or `from` if there is none (so the
  /// span before it is empty).
  pure def lastSlash(t: Text, from: int): int =
    largest(t.indices().filter(i => i >= from and t[i] == "/"), from)

  // ---------- parsing (`Policy::parse`) ----------

  /// A list of strings, or None for anything else (null, a null element, another type).
  pure def strings(f: Field): { ok: bool, v: List[str] } = match f {
    | FTexts(l) => { ok: true, v: l }
    | _ => { ok: false, v: [] }
  }

  pure def ruleStrings(f: RuleField): { ok: bool, v: List[str] } = match f {
    | RTexts(l) => { ok: true, v: l }
    | _ => { ok: false, v: [] }
  }

  /// An optional list-of-strings field: absent means `absent`.
  pure def listField(d: Doc, name: str, absent: List[str]): { ok: bool, v: List[str] } =
    if (d.keys().contains(name)) strings(d.get(name)) else { ok: true, v: absent }

  pure def ruleList(f: str -> RuleField, name: str): { ok: bool, v: List[str] } =
    if (f.keys().contains(name)) ruleStrings(f.get(name)) else { ok: true, v: [] }

  pure def approvalsOk(f: str -> RuleField): bool =
    not(f.keys().contains("approvals")) or match f.get("approvals") {
      | RInt(n) => n <= U32_MAX
      | _ => false
    }

  pure def ruleOk(r: RuleDoc): bool = match r {
    | RuleNull => false
    | RuleObject(f) => ruleList(f, "writers").ok and ruleList(f, "required_checks").ok and approvalsOk(f)
  }

  pure def ruleOf(r: RuleDoc): Rule = match r {
    | RuleNull => { writers: [], approvals: NoApprovals, requiredChecks: [] }
    | RuleObject(f) => {
      writers: ruleList(f, "writers").v,
      approvals:
        if (f.keys().contains("approvals"))
          match f.get("approvals") { | RInt(n) => NeedsApprovals(n) | _ => NoApprovals }
        else NoApprovals,
      requiredChecks: ruleList(f, "required_checks").v,
    }
  }

  /// An exact name or a single trailing `*`.
  pure def globOk(g: Text): bool =
    g.length() > 0 and g.indices().forall(i => i == g.length() - 1 or g[i] != "*")

  pure def rulesOf(d: Doc): { ok: bool, v: Text -> RuleDoc } =
    if (not(d.keys().contains("branches"))) { ok: true, v: Map() }
    else match d.get("branches") {
      | FRules(m) => { ok: true, v: m }
      | _ => { ok: false, v: Map() }
    }

  pure def parse(d: Doc): Loaded = {
    val readers = listField(d, "readers", ["*"])
    val admins = listField(d, "admins", [])
    val gc = listField(d, "gc", [])
    val rules = rulesOf(d)
    val ok = and {
      d.keys().contains("version") and d.get("version") == FInt(1),
      readers.ok,
      admins.ok,
      gc.ok,
      rules.ok,
      rules.v.keys().forall(g => ruleOk(rules.v.get(g)) and globOk(g)),
    }
    if (ok)
      Valid({
        readers: readers.v,
        admins: admins.v,
        gc: gc.v,
        branches: rules.v.keys().mapBy(g => ruleOf(rules.v.get(g))),
      })
    else Invalid
  }

  // ---------- deciding (`Policy::decide`) ----------

  pure def isAny(c: Caller, who: List[str]): bool =
    who.indices().exists(i => who[i] == "*" or who[i] == c.principal or c.groups.contains(who[i]))

  /// A branch name: the span `[start, end)` of a key.
  type Span = { start: int, end: int }

  /// Whether glob `g` names the branch at `b` in `key` exactly.
  pure def namesExactly(g: Text, key: Text, b: Span): bool =
    b.end - b.start == g.length() and occursAt(key, b.start, g)

  /// Whether `g` ends in `*` and the branch at `b` starts with the rest of it.
  pure def namesByPrefix(g: Text, key: Text, b: Span): bool = and {
    g.length() > 0,
    g[g.length() - 1] == "*",
    b.end - b.start >= g.length() - 1,
    g.indices().forall(i => i == g.length() - 1 or key[b.start + i] == g[i]),
  }

  /// Whether the rule for the branch at `b` in `key` lets `c` write it. The rule is the
  /// exact one, else the longest trailing-`*` prefix (matching globs of equal length are
  /// prefixes of the branch of equal length, so equal); no rule lets nobody write.
  ///
  /// Values used more than once are bound by a lambda (`Set(v).exists(x => ...)`): the
  /// generated obligations inline every definition, and a bound name keeps them small.
  pure def ruleLets(p: Policy, c: Caller, key: Text, b: Span): bool =
    Set(p.branches.keys().filter(g => namesExactly(g, key, b))).exists(exact =>
      if (exact != Set()) exact.exists(g => isAny(c, p.branches.get(g).writers))
      else Set(p.branches.keys().filter(g => namesByPrefix(g, key, b))).exists(prefixes =>
        Set(largest(prefixes.map(g => g.length()), 0)).exists(longest =>
          prefixes.exists(g => g.length() == longest and isAny(c, p.branches.get(g).writers))
        )
      )
    )

  pure def writesAnything(p: Policy, c: Caller): bool =
    p.branches.keys().exists(g => isAny(c, p.branches.get(g).writers))

  /// The branch a branch object belongs to, if `key` is one: `refs/<b>`,
  /// `intents/<b>/<id>`, `reflog/<b>/<ts>`.
  pure def isBranchKey(key: Text): bool =
    startsWith(key, REFS) or startsWith(key, INTENTS) or startsWith(key, REFLOG)

  /// The branch of a branch object (`isBranchKey`).
  pure def branchOf(key: Text): Span =
    if (startsWith(key, REFS)) { start: REFS.length(), end: key.length() }
    else if (startsWith(key, INTENTS)) { start: INTENTS.length(), end: lastSlash(key, INTENTS.length()) }
    else { start: REFLOG.length(), end: lastSlash(key, REFLOG.length()) }

  pure def allows(p: Policy, c: Caller, op: Op, key: Text): bool =
    if (op == Read) isAny(c, p.readers)
    else if (isBranchKey(key)) Set(branchOf(key)).exists(b => ruleLets(p, c, key, b))
    else if (startsWith(key, LOCKS)) {
      if (occursAt(key, LOCKS.length(), SHARED)) writesAnything(p, c)
      else occursAt(key, LOCKS.length(), EXCLUSIVE) and isAny(c, p.gc)
    }
    else if (isContent(key)) {
      if (op == Delete) isAny(c, p.gc) else writesAnything(p, c)
    }
    else if (key == HEAD) isAny(c, p.admins)
    else false

  // ---------- requests (`request_op`) ----------

  /// `inm` is the If-None-Match header, "" when absent.
  pure def requestOpOf(method: str, key: Text, inm: str): Op =
    if (method == "GET" or method == "HEAD") Read
    else if (method == "PUT" and isContent(key) and inm != "*") Delete
    else if (method == "DELETE" and isContent(key)) Delete
    else Write

  // ---------- actions ----------

  var state: State

  action init = state' = { policy: NoPolicy, answer: NoAnswer }

  /// Read `policy.json`.
  action load(d: Doc): bool =
    state' = { policy: Set(d).fold(NoPolicy, (none, doc) => parse(doc)), answer: NoAnswer }

  /// May `c` do `op` to `key`, under the loaded policy?
  action decide(c: Caller, op: Op, key: Text): bool = all {
    match state.policy { | Valid(p) => true | _ => false },
    state' = {
      ...state,
      answer: match state.policy {
        | Valid(p) =>
          if (Set(c).exists(who => Set(key).exists(k => allows(p, who, op, k)))) Allowed else Denied
        | _ => NoAnswer
      },
    },
  }

  /// Which op a request is.
  action requestOp(method: str, key: Text, inm: str): bool =
    state' = { ...state, answer: Mapped(Set(key).fold(Read, (none, k) => requestOpOf(method, k, inm))) }
}
