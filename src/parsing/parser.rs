use super::syntax_definition::*;
use super::scope::*;
use onig::{self, Region};
use std::usize;
use std::collections::{HashMap, HashSet};
use std::i32;
use std::hash::BuildHasherDefault;
use fnv::FnvHasher;

/// Keeps the current parser state (the internal syntax interpreter stack) between lines of parsing.
/// If you are parsing an entire file you create one of these at the start and use it
/// all the way to the end.
///
/// # Caching
///
/// One reason this is exposed is that since it implements `Clone` you can actually cache
/// these (probably along with a `HighlightState`) and only re-start parsing from the point of a change.
/// See the docs for `HighlightState` for more in-depth discussion of caching.
///
/// This state doesn't keep track of the current scope stack and parsing only returns changes to this stack
/// so if you want to construct scope stacks you'll need to keep track of that as well.
/// Note that `HighlightState` contains exactly this as a public field that you can use.
///
/// **Note:** Caching is for advanced users who have tons of time to maximize performance or want to do so eventually.
/// It is not recommended that you try caching the first time you implement highlighting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseState {
    stack: Vec<StateLevel>,
    first_line: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StateLevel {
    context: ContextPtr,
    prototype: Option<ContextPtr>,
    captures: Option<(Region, String)>,
}

#[derive(Debug)]
struct RegexMatch {
    regions: Region,
    context: ContextPtr,
    pat_index: usize,
    match_ptr: *const MatchPattern,
}

/// maps the pattern to the start index, which is -1 if not found.
type SearchCache = HashMap<*const MatchPattern, Option<Region>, BuildHasherDefault<FnvHasher>>;
type MatchedPatterns = HashSet<*const MatchPattern, BuildHasherDefault<FnvHasher>>;

impl ParseState {
    /// Create a state from a syntax, keeps its own reference counted
    /// pointer to the main context of the syntax.
    pub fn new(syntax: &SyntaxDefinition) -> ParseState {
        let start_state = StateLevel {
            context: syntax.contexts["main"].clone(),
            prototype: None,
            captures: None,
        };
        ParseState {
            stack: vec![start_state],
            first_line: true,
        }
    }

    /// Parses a single line of the file. Because of the way regex engines work you unfortunately
    /// have to pass in a single line contigous in memory. This can be bad for really long lines.
    /// Sublime Text avoids this by just not highlighting lines that are too long (thousands of characters).
    ///
    /// For efficiency reasons this returns only the changes to the current scope at each point in the line.
    /// You can use `ScopeStack#apply` on each operation in succession to get the stack for a given point.
    /// Look at the code in `highlighter.rs` for an example of doing this for highlighting purposes.
    ///
    /// The vector is in order both by index to apply at (the `usize`) and also by order to apply them at a
    /// given index (e.g popping old scopes before pusing new scopes).
    pub fn parse_line(&mut self, line: &str) -> Vec<(usize, ScopeStackOp)> {
        assert!(self.stack.len() > 0,
                "Somehow main context was popped from the stack");
        let mut match_start = 0;
        let mut prev_match_start = 0;
        let mut res = Vec::new();

        if self.first_line {
            let cur_level = &self.stack[self.stack.len() - 1];
            let context = cur_level.context.borrow();
            if !context.meta_content_scope.is_empty() {
                res.push((0, ScopeStackOp::Push(context.meta_content_scope[0])));
            }
            self.first_line = false;
        }

        let mut regions = Region::with_capacity(8);
        let fnv = BuildHasherDefault::<FnvHasher>::default();
        let mut search_cache: SearchCache = HashMap::with_capacity_and_hasher(128, fnv);
        let fnv2 = BuildHasherDefault::<FnvHasher>::default();
        // Fixes issue https://github.com/trishume/syntect/issues/25
        let mut matched: MatchedPatterns = HashSet::with_capacity_and_hasher(4, fnv2);

        while self.parse_next_token(line,
                                    &mut match_start,
                                    &mut search_cache,
                                    &mut matched,
                                    &mut regions,
                                    &mut res) {
            // We only care about not repeatedly matching things at the same location
            if match_start != prev_match_start {
                matched.clear();
            }
            prev_match_start = match_start;
        }

        res
    }

    fn parse_next_token(&mut self,
                        line: &str,
                        start: &mut usize,
                        search_cache: &mut SearchCache,
                        matched: &mut MatchedPatterns,
                        regions: &mut Region,
                        ops: &mut Vec<(usize, ScopeStackOp)>)
                        -> bool {
        let cur_match = {
            let cur_level = &self.stack[self.stack.len() - 1];
            let mut min_start = usize::MAX;
            let mut cur_match: Option<RegexMatch> = None;
            let prototype: Option<ContextPtr> = {
                let ctx_ref = cur_level.context.borrow();
                ctx_ref.prototype.clone()
            };
            let context_chain = self.stack
                .iter()
                .filter_map(|lvl| lvl.prototype.as_ref().cloned())
                .chain(prototype.into_iter())
                .chain(Some(cur_level.context.clone()).into_iter());
            // println!("{:#?}", cur_level);
            // println!("token at {} on {}", start, line.trim_right());
            for ctx in context_chain {
                for (pat_context_ptr, pat_index) in context_iter(ctx) {
                    let mut pat_context = pat_context_ptr.borrow_mut();
                    let mut match_pat = pat_context.match_at_mut(pat_index);
                    // println!("{} - {:?} - {:?}", match_pat.regex_str, match_pat.has_captures, cur_level.captures.is_some());
                    let match_ptr = match_pat as *const MatchPattern;

                    // Avoid matching the same pattern twice in the same place, causing an infinite loop
                    if matched.contains(&match_ptr) {
                        continue;
                    }

                    if let Some(maybe_region) =
                           search_cache.get(&match_ptr) {
                        let mut valid_entry = true;
                        if let Some(ref region) = *maybe_region {
                            let match_start = region.pos(0).unwrap().0;
                            if match_start < *start {
                                valid_entry = false;
                            } else if match_start < min_start {
                                // print!("match {} at {} on {}", match_pat.regex_str, match_start, line);
                                min_start = match_start;
                                cur_match = Some(RegexMatch {
                                    regions: region.clone(),
                                    context: pat_context_ptr.clone(),
                                    pat_index: pat_index,
                                    match_ptr: match_ptr,
                                });
                            }
                        }
                        if valid_entry {
                            continue;
                        }
                    }

                    match_pat.ensure_compiled_if_possible();
                    let refs_regex = if match_pat.has_captures && cur_level.captures.is_some() {
                        let &(ref region, ref s) = cur_level.captures.as_ref().unwrap();
                        match match_pat.compile_with_refs(region, s) {
                            Ok(m)  => Some(m),
                            Err(_) => None,
                        }
                    } else {
                        None
                    };
                    let regex = if let Some(ref rgx) = refs_regex {
                        Some(rgx)
                    } else {
                        if match_pat.regex.is_some() {
                            Some(match_pat.regex.as_ref().unwrap())
                        } else {
                            None
                        }
                    };

                    let matched = if let Some(r) = regex {
                        r.search_with_options(line,
                                              *start,
                                              line.len(),
                                              onig::SEARCH_OPTION_NONE,
                                              Some(regions))
                    } else {
                        None
                    };

                    if let Some(match_start) = matched {
                        let match_end = regions.pos(0).unwrap().1;
                        // this is necessary to avoid infinite looping on dumb patterns
                        let does_something = match match_pat.operation {
                            MatchOperation::None => match_start != match_end,
                            _ => true,
                        };
                        if refs_regex.is_none() && does_something {
                            search_cache.insert(match_pat, Some(regions.clone()));
                        }
                        if match_start < min_start && does_something {
                            // print!("catch {} at {} on {}", match_pat.regex_str, match_start, line);
                            min_start = match_start;
                            cur_match = Some(RegexMatch {
                                regions: regions.clone(),
                                context: pat_context_ptr.clone(),
                                pat_index: pat_index,
                                match_ptr: match_ptr,
                            });
                        }
                    } else if refs_regex.is_none() {
                        search_cache.insert(match_pat, None);
                    }
                }
            }
            cur_match
        };

        if let Some(reg_match) = cur_match {
            let (_, match_end) = reg_match.regions.pos(0).unwrap();
            *start = match_end;
            let level_context = self.stack[self.stack.len() - 1].context.clone();
            matched.insert(reg_match.match_ptr);
            self.exec_pattern(line, reg_match, level_context, ops);
            true
        } else {
            false
        }
    }

    /// Returns true if the stack was changed
    fn exec_pattern(&mut self,
                    line: &str,
                    reg_match: RegexMatch,
                    level_context_ptr: ContextPtr,
                    ops: &mut Vec<(usize, ScopeStackOp)>)
                    -> bool {
        let (match_start, match_end) = reg_match.regions.pos(0).unwrap();
        let context = reg_match.context.borrow();
        let pat = context.match_at(reg_match.pat_index);
        let level_context = level_context_ptr.borrow();
        // println!("running pattern {:?} on '{}' at {}", pat.regex_str, line, match_start);

        self.push_meta_ops(true, match_start, &*level_context, &pat.operation, ops);
        for s in &pat.scope {
            // println!("pushing {:?} at {}", s, match_start);
            ops.push((match_start, ScopeStackOp::Push(*s)));
        }
        if let Some(ref capture_map) = pat.captures {
            // captures could appear in an arbitrary order, have to produce ops in right order
            // ex: ((bob)|(hi))* could match hibob in wrong order, and outer has to push first
            // we don't have to handle a capture matching multiple times, Sublime doesn't
            let mut map: Vec<((usize, i32), ScopeStackOp)> = Vec::new();
            for (cap_index, scopes) in capture_map.iter() {
                if let Some((cap_start, cap_end)) = reg_match.regions.pos(*cap_index) {
                    // marking up empty captures causes pops to be sorted wrong
                    if cap_start == cap_end {
                        continue;
                    }
                    // println!("capture {:?} at {:?}-{:?}", scopes[0], cap_start, cap_end);
                    for scope in scopes.iter() {
                        map.push(((cap_start, -((cap_end - cap_start) as i32)),
                                  ScopeStackOp::Push(*scope)));
                    }
                    map.push(((cap_end, i32::MIN), ScopeStackOp::Pop(scopes.len())));
                }
            }
            map.sort_by(|a, b| a.0.cmp(&b.0));
            for ((index, _), op) in map.into_iter() {
                ops.push((index, op));
            }
        }
        if !pat.scope.is_empty() {
            // println!("popping at {}", match_end);
            ops.push((match_end, ScopeStackOp::Pop(pat.scope.len())));
        }
        self.push_meta_ops(false, match_end, &*level_context, &pat.operation, ops);

        self.perform_op(line, &reg_match.regions, pat)
    }

    fn push_meta_ops(&self,
                     initial: bool,
                     index: usize,
                     cur_context: &Context,
                     match_op: &MatchOperation,
                     ops: &mut Vec<(usize, ScopeStackOp)>) {
        let involves_pop = match *match_op {
            MatchOperation::Pop |
            MatchOperation::Set(_) => true,
            MatchOperation::Push(_) |
            MatchOperation::None => false,
        };
        // println!("metas ops for {:?}, is pop: {}, initial: {}",
        //          match_op,
        //          involves_pop,
        //          initial);
        // println!("{:?}", cur_context.meta_scope);
        if involves_pop {
            let v = if initial {
                &cur_context.meta_content_scope
            } else {
                &cur_context.meta_scope
            };
            if !v.is_empty() {
                ops.push((index, ScopeStackOp::Pop(v.len())));
            }
        }
        match *match_op {
            MatchOperation::Push(ref context_refs) |
            MatchOperation::Set(ref context_refs) => {
                for r in context_refs {
                    let ctx_ptr = r.resolve();
                    let ctx = ctx_ptr.borrow();
                    let v = if initial {
                        &ctx.meta_scope
                    } else {
                        &ctx.meta_content_scope
                    };
                    for scope in v.iter() {
                        ops.push((index, ScopeStackOp::Push(*scope)));
                    }
                }
            }
            MatchOperation::None |
            MatchOperation::Pop => (),
        }
    }

    /// Returns true if the stack was changed
    fn perform_op(&mut self, line: &str, regions: &Region, pat: &MatchPattern) -> bool {
        let ctx_refs = match pat.operation {
            MatchOperation::Push(ref ctx_refs) => ctx_refs,
            MatchOperation::Set(ref ctx_refs) => {
                self.stack.pop();
                ctx_refs
            }
            MatchOperation::Pop => {
                self.stack.pop();
                return true;
            }
            MatchOperation::None => return false,
        };
        for (i, r) in ctx_refs.iter().enumerate() {
            let proto = if i == 0 {
                pat.with_prototype.clone()
            } else {
                None
            };
            let ctx_ptr = r.resolve();
            let captures = {
                let ctx = ctx_ptr.borrow();
                if ctx.uses_backrefs {
                    Some((regions.clone(), line.to_owned()))
                } else {
                    None
                }
            };
            self.stack.push(StateLevel {
                context: ctx_ptr,
                prototype: proto,
                captures: captures,
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parsing::{SyntaxSet, Scope, ScopeStack};
    use util::debug_print_ops;

    #[test]
    fn can_parse() {
        use parsing::ScopeStackOp::{Push, Pop};
        let ps = SyntaxSet::load_from_folder("testdata/Packages").unwrap();
        let mut state = {
            let syntax = ps.find_syntax_by_name("Ruby on Rails").unwrap();
            ParseState::new(syntax)
        };
        let mut state2 = {
            let syntax = ps.find_syntax_by_name("HTML (Rails)").unwrap();
            ParseState::new(syntax)
        };
        let mut state3 = {
            let syntax = ps.find_syntax_by_name("C").unwrap();
            ParseState::new(syntax)
        };

        let line = "module Bob::Wow::Troll::Five; 5; end";
        let ops = state.parse_line(line);
        debug_print_ops(line, &ops);

        let test_ops = vec![
            (0, Push(Scope::new("source.ruby.rails").unwrap())),
            (0, Push(Scope::new("meta.module.ruby").unwrap())),
            (0, Push(Scope::new("keyword.control.module.ruby").unwrap())),
            (6, Pop(1)),
            (7, Push(Scope::new("entity.name.type.module.ruby").unwrap())),
            (7, Push(Scope::new("entity.other.inherited-class.module.first.ruby").unwrap())),
            (10, Push(Scope::new("punctuation.separator.inheritance.ruby").unwrap())),
            (12, Pop(1)),
            (12, Pop(1)),
        ];
        assert_eq!(&ops[0..test_ops.len()], &test_ops[..]);

        let line2 = "def lol(wow = 5)";
        let ops2 = state.parse_line(line2);
        debug_print_ops(line2, &ops2);
        let test_ops2 =
            vec![(0, Push(Scope::new("meta.function.method.with-arguments.ruby").unwrap())),
                 (0, Push(Scope::new("keyword.control.def.ruby").unwrap())),
                 (3, Pop(1)),
                 (4, Push(Scope::new("entity.name.function.ruby").unwrap())),
                 (7, Pop(1)),
                 (7, Push(Scope::new("punctuation.definition.parameters.ruby").unwrap())),
                 (8, Pop(1)),
                 (8, Push(Scope::new("variable.parameter.function.ruby").unwrap())),
                 (12, Push(Scope::new("keyword.operator.assignment.ruby").unwrap())),
                 (13, Pop(1)),
                 (14, Push(Scope::new("constant.numeric.ruby").unwrap())),
                 (15, Pop(1)),
                 (15, Pop(1)),
                 (15, Push(Scope::new("punctuation.definition.parameters.ruby").unwrap())),
                 (16, Pop(1)),
                 (16, Pop(1))];
        assert_eq!(ops2, test_ops2);

        let line3 = "<script>var lol = '<% def wow(";
        let ops3 = state2.parse_line(line3);
        debug_print_ops(line3, &ops3);
        let mut test_stack = ScopeStack::new();
        test_stack.push(Scope::new("text.html.ruby").unwrap());
        test_stack.push(Scope::new("text.html.basic").unwrap());
        test_stack.push(Scope::new("source.js.embedded.html").unwrap());
        test_stack.push(Scope::new("string.quoted.single.js").unwrap());
        test_stack.push(Scope::new("source.ruby.rails.embedded.html").unwrap());
        test_stack.push(Scope::new("meta.function.method.with-arguments.ruby").unwrap());
        test_stack.push(Scope::new("variable.parameter.function.ruby").unwrap());
        let mut test_stack2 = ScopeStack::new();
        for &(_, ref op) in ops3.iter() {
            test_stack2.apply(op);
        }
        assert_eq!(test_stack2, test_stack);

        // for testing backrefs
        let line4 = "lol = <<-END wow END";
        let ops4 = state.parse_line(line4);
        debug_print_ops(line4, &ops4);
        let test_ops4 = vec![
            (4, Push(Scope::new("keyword.operator.assignment.ruby").unwrap())),
            (5, Pop(1)),
            (6, Push(Scope::new("string.unquoted.heredoc.ruby").unwrap())),
            (6, Push(Scope::new("punctuation.definition.string.begin.ruby").unwrap())),
            (12, Pop(1)),
            (16, Push(Scope::new("punctuation.definition.string.end.ruby").unwrap())),
            (20, Pop(1)),
            (20, Pop(1)),
        ];
        assert_eq!(ops4, test_ops4);

        // test fix for issue #25
        let line5 = "struct{estruct";
        let ops5 = state3.parse_line(line5);
        assert_eq!(ops5.len(), 10);

        // assert!(false);
    }
}
