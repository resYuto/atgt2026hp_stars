/// Lazy graph-reduction SKI combinator evaluator.
///
/// Arena-based allocation with sharing via indirection nodes.
/// Reads compact format (k=S, X=K, D=I, -=application).
use std::env;
use std::fs;
use std::io::{self, Write};
use std::process;

// Node tags
const APP: u8 = 0;
const S: u8 = 1;
const K: u8 = 2;
const I: u8 = 3;
const S1: u8 = 4; // S applied to 1 arg
const S2: u8 = 5; // S applied to 2 args
const K1: u8 = 6; // K applied to 1 arg
const IND: u8 = 7; // Indirection (sharing/update)

const NIL: u32 = u32::MAX;
const ARENA_HARD_LIMIT: usize = 1_900_000_000;
const IO_ALLOC_LIMIT_GROWTH_MULTIPLIER: usize = 8;
const IO_ALLOC_LIMIT_MIN_HEADROOM: usize = 50_000_000;

#[derive(Clone, Copy)]
struct Node {
    tag: u8,
    a: u32, // left child / first arg / indirection target
    b: u32, // right child / second arg
}

struct Arena {
    nodes: Vec<Node>,
    free_list: Vec<u32>,
    gc_roots: Vec<u32>, // external roots for GC
    cached_k: Option<u32>,
    cached_i: Option<u32>,
    cached_ki: Option<u32>,
    cached_marker_t: Option<u32>,
    cached_marker_f: Option<u32>,
    cached_diamond_sels: [Option<u32>; 5],
    io_alloc_failsafe_limit: Option<usize>,
    // Checkpoint/restore for per-pixel rendering
    checkpoint: Option<usize>,     // arena length at checkpoint
    saved_nodes: Vec<(u32, Node)>, // base nodes modified since checkpoint
}

impl Arena {
    fn new(capacity: usize) -> Self {
        Arena {
            nodes: Vec::with_capacity(capacity),
            free_list: Vec::new(),
            gc_roots: Vec::new(),
            cached_k: None,
            cached_i: None,
            cached_ki: None,
            cached_marker_t: None,
            cached_marker_f: None,
            cached_diamond_sels: [None; 5],
            io_alloc_failsafe_limit: None,
            checkpoint: None,
            saved_nodes: Vec::new(),
        }
    }

    #[inline]
    fn enable_io_alloc_failsafe(&mut self, limit: usize) {
        self.io_alloc_failsafe_limit = Some(limit.clamp(1, ARENA_HARD_LIMIT));
    }

    #[inline]
    fn alloc(&mut self, tag: u8, a: u32, b: u32) -> u32 {
        // During checkpoint mode, don't use free list (all new allocs go to end
        // so they can be truncated on restore)
        if self.checkpoint.is_none() {
            if let Some(idx) = self.free_list.pop() {
                self.nodes[idx as usize] = Node { tag, a, b };
                return idx;
            }
        }
        // Arena size limit: 1.9B nodes (~22.8GB) — push memory to the max
        if self.nodes.len() >= ARENA_HARD_LIMIT {
            eprintln!("ARENA LIMIT: {} nodes reached, aborting", self.nodes.len());
            std::process::exit(1);
        }
        if let Some(limit) = self.io_alloc_failsafe_limit {
            if self.nodes.len() >= limit {
                let node_bytes = std::mem::size_of::<Node>() as u128;
                let approx_mib = (self.nodes.len() as u128 * node_bytes) / (1024 * 1024);
                eprintln!(
                    "IO ALLOC FAILSAFE: nodes={} reached limit={} (~{} MiB node storage, free_list={}, checkpoint={}), aborting",
                    self.nodes.len(),
                    limit,
                    approx_mib,
                    self.free_list.len(),
                    self.checkpoint.is_some()
                );
                std::process::exit(1);
            }
        }
        if self.nodes.len() == self.nodes.capacity() {
            if let Err(err) = self.nodes.try_reserve(1) {
                eprintln!(
                    "ALLOC FAILSAFE: reserve failed at nodes={} capacity={} free_list={} error={:?}",
                    self.nodes.len(),
                    self.nodes.capacity(),
                    self.free_list.len(),
                    err
                );
                std::process::exit(1);
            }
        }
        let idx = self.nodes.len() as u32;
        self.nodes.push(Node { tag, a, b });
        idx
    }

    /// Save a node's current state before modifying it (for checkpoint/restore)
    #[inline]
    fn save_node(&mut self, idx: u32) {
        if let Some(cp) = self.checkpoint {
            if (idx as usize) < cp {
                self.saved_nodes.push((idx, self.nodes[idx as usize]));
            }
        }
    }

    #[inline]
    fn keep_root(&mut self, idx: u32) {
        if !self.gc_roots.contains(&idx) {
            self.gc_roots.push(idx);
        }
    }

    #[inline]
    fn valid_cached(&self, idx: Option<u32>) -> Option<u32> {
        match idx {
            Some(i) if (i as usize) < self.nodes.len() => Some(i),
            _ => None,
        }
    }

    fn intern_k(&mut self) -> u32 {
        if let Some(idx) = self.valid_cached(self.cached_k) {
            return idx;
        }
        let idx = self.alloc(K, NIL, NIL);
        if self.checkpoint.is_none() {
            self.cached_k = Some(idx);
            self.keep_root(idx);
        }
        idx
    }

    fn intern_i(&mut self) -> u32 {
        if let Some(idx) = self.valid_cached(self.cached_i) {
            return idx;
        }
        let idx = self.alloc(I, NIL, NIL);
        if self.checkpoint.is_none() {
            self.cached_i = Some(idx);
            self.keep_root(idx);
        }
        idx
    }

    fn intern_ki(&mut self) -> u32 {
        if let Some(idx) = self.valid_cached(self.cached_ki) {
            return idx;
        }
        let i_node = self.intern_i();
        let idx = self.alloc(K1, i_node, NIL); // KI
        if self.checkpoint.is_none() {
            self.cached_ki = Some(idx);
            self.keep_root(idx);
        }
        idx
    }

    fn intern_marker_t(&mut self) -> u32 {
        if let Some(idx) = self.valid_cached(self.cached_marker_t) {
            return idx;
        }
        let idx = self.alloc(100, NIL, NIL);
        if self.checkpoint.is_none() {
            self.cached_marker_t = Some(idx);
            self.keep_root(idx);
        }
        idx
    }

    fn intern_marker_f(&mut self) -> u32 {
        if let Some(idx) = self.valid_cached(self.cached_marker_f) {
            return idx;
        }
        let idx = self.alloc(101, NIL, NIL);
        if self.checkpoint.is_none() {
            self.cached_marker_f = Some(idx);
            self.keep_root(idx);
        }
        idx
    }

    fn intern_diamond_sel(&mut self, pos: usize) -> u32 {
        if pos >= 5 {
            panic!("selector position out of range: {}", pos);
        }
        if let Some(idx) = self.valid_cached(self.cached_diamond_sels[pos]) {
            return idx;
        }

        // core_4 = I
        // core_n = S(KK)(core_{n+1})
        // sel_i = K^i(core_i)
        let i_node = self.intern_i();
        let k_node = self.intern_k();
        let kk = self.alloc(K1, k_node, NIL); // K(K)

        let mut core = i_node;
        for _ in 0..(4 - pos) {
            core = self.alloc(S2, kk, core); // S(KK)(core)
        }

        let mut result = core;
        for _ in 0..pos {
            result = self.alloc(K1, result, NIL); // K(result)
        }

        if self.checkpoint.is_none() {
            self.cached_diamond_sels[pos] = Some(result);
            self.keep_root(result);
        }
        result
    }

    // /// Set a checkpoint: record current arena length for later restore
    // fn set_checkpoint(&mut self) {
    //     self.checkpoint = Some(self.nodes.len());
    //     self.saved_nodes.clear();
    // }

    // /// Restore arena to checkpoint state: undo all base node modifications
    // /// and truncate new allocations
    // fn restore_checkpoint(&mut self) {
    //     if let Some(cp) = self.checkpoint {
    //         // Restore modified base nodes in reverse order
    //         for (idx, node) in self.saved_nodes.drain(..).rev() {
    //             self.nodes[idx as usize] = node;
    //         }
    //         // Truncate temporary allocations
    //         self.nodes.truncate(cp);
    //         self.checkpoint = None;
    //     }
    // }

    /// Mark-sweep garbage collection.
    /// `roots` are the node indices that must be kept alive.
    /// Returns (total_nodes, live_nodes, freed_nodes).
    fn gc(&mut self, roots: &[u32]) -> (usize, usize, usize) {
        let len = self.nodes.len();
        // Bitmap: 1 bit per node. ~62.5MB for 500M nodes.
        let mut marked = vec![0u64; (len + 63) / 64];

        #[inline]
        fn is_marked(marked: &[u64], idx: u32) -> bool {
            let i = idx as usize;
            (marked[i / 64] >> (i % 64)) & 1 != 0
        }
        #[inline]
        fn set_mark(marked: &mut [u64], idx: u32) {
            let i = idx as usize;
            marked[i / 64] |= 1u64 << (i % 64);
        }

        // Mark phase: iterative DFS
        let mut stack: Vec<u32> = Vec::with_capacity(1024);
        for &r in roots.iter().chain(self.gc_roots.iter()) {
            if (r as usize) < len && !is_marked(&marked, r) {
                stack.push(r);
            }
        }

        while let Some(idx) = stack.pop() {
            if is_marked(&marked, idx) {
                continue;
            }
            set_mark(&mut marked, idx);
            let node = self.nodes[idx as usize];
            if node.a != NIL && (node.a as usize) < len && !is_marked(&marked, node.a) {
                stack.push(node.a);
            }
            if node.b != NIL && (node.b as usize) < len && !is_marked(&marked, node.b) {
                stack.push(node.b);
            }
        }

        // Sweep phase: build free list from unmarked nodes
        self.free_list.clear();
        let mut live = 0usize;
        for i in 0..len {
            if is_marked(&marked, i as u32) {
                live += 1;
            } else {
                self.free_list.push(i as u32);
            }
        }
        let freed = len - live;
        (len, live, freed)
    }

    #[inline]
    fn follow(&self, mut idx: u32) -> u32 {
        loop {
            let n = &self.nodes[idx as usize];
            if n.tag != IND {
                return idx;
            }
            idx = n.a;
        }
    }

    /// Follow and also do path compression (skipped during checkpoint mode).
    #[inline]
    fn follow_mut(&mut self, idx: u32) -> u32 {
        let root = self.follow(idx);
        // Skip path compression during checkpoint mode to avoid modifying base nodes
        if self.checkpoint.is_some() {
            return root;
        }
        // Path compression
        let mut cur = idx;
        while cur != root {
            let n = &self.nodes[cur as usize];
            if n.tag != IND {
                break;
            }
            let next = n.a;
            self.nodes[cur as usize].a = root;
            cur = next;
        }
        root
    }

    /// Reduce to Weak Head Normal Form.
    fn whnf(&mut self, node: u32, fuel: &mut u64) -> u32 {
        let mut spine: Vec<u32> = Vec::with_capacity(256);
        let mut n = self.follow_mut(node);

        loop {
            if *fuel == 0 {
                return n;
            }

            let tag = self.nodes[n as usize].tag;

            match tag {
                APP => {
                    spine.push(n);
                    let a = self.nodes[n as usize].a;
                    n = self.follow_mut(a);
                    continue;
                }
                I if !spine.is_empty() => {
                    // I x -> x
                    *fuel -= 1;
                    let app = spine.pop().unwrap();
                    let x = self.follow_mut(self.nodes[app as usize].b);
                    self.save_node(app);
                    self.nodes[app as usize].tag = IND;
                    self.nodes[app as usize].a = x;
                    n = x;
                    continue;
                }
                K if spine.len() >= 2 => {
                    // K x y -> x
                    *fuel -= 1;
                    let app1 = spine.pop().unwrap(); // K x
                    let app2 = spine.pop().unwrap(); // (K x) y
                    let x = self.follow_mut(self.nodes[app1 as usize].b);
                    self.save_node(app2);
                    self.nodes[app2 as usize].tag = IND;
                    self.nodes[app2 as usize].a = x;
                    self.save_node(app1);
                    self.nodes[app1 as usize].tag = K1;
                    self.nodes[app1 as usize].a = x;
                    n = x;
                    continue;
                }
                K1 if !spine.is_empty() => {
                    // (K x) y -> x
                    *fuel -= 1;
                    let app = spine.pop().unwrap();
                    let x = self.follow_mut(self.nodes[n as usize].a);
                    self.save_node(app);
                    self.nodes[app as usize].tag = IND;
                    self.nodes[app as usize].a = x;
                    n = x;
                    continue;
                }
                S if spine.len() >= 3 => {
                    // S f g x -> f x (g x)
                    *fuel -= 1;
                    let app1 = spine.pop().unwrap(); // S f
                    let app2 = spine.pop().unwrap(); // (S f) g
                    let app3 = spine.pop().unwrap(); // ((S f) g) x
                    let f = self.follow_mut(self.nodes[app1 as usize].b);
                    let g = self.follow_mut(self.nodes[app2 as usize].b);
                    let x = self.nodes[app3 as usize].b; // keep sharing
                    let fx = self.alloc(APP, f, x);
                    let gx = self.alloc(APP, g, x);
                    let result = self.alloc(APP, fx, gx);
                    self.save_node(app3);
                    self.nodes[app3 as usize].tag = IND;
                    self.nodes[app3 as usize].a = result;
                    self.save_node(app1);
                    self.nodes[app1 as usize].tag = S1;
                    self.nodes[app1 as usize].a = f;
                    self.save_node(app2);
                    self.nodes[app2 as usize].tag = S2;
                    self.nodes[app2 as usize].a = f;
                    self.nodes[app2 as usize].b = g;
                    n = result;
                    continue;
                }
                S1 if spine.len() >= 2 => {
                    // (S f) g x -> f x (g x)
                    *fuel -= 1;
                    let app1 = spine.pop().unwrap(); // (S f) g
                    let app2 = spine.pop().unwrap(); // ((S f) g) x
                    let f = self.follow_mut(self.nodes[n as usize].a);
                    let g = self.follow_mut(self.nodes[app1 as usize].b);
                    let x = self.nodes[app2 as usize].b;
                    let fx = self.alloc(APP, f, x);
                    let gx = self.alloc(APP, g, x);
                    let result = self.alloc(APP, fx, gx);
                    self.save_node(app2);
                    self.nodes[app2 as usize].tag = IND;
                    self.nodes[app2 as usize].a = result;
                    self.save_node(app1);
                    self.nodes[app1 as usize].tag = S2;
                    self.nodes[app1 as usize].a = f;
                    self.nodes[app1 as usize].b = g;
                    n = result;
                    continue;
                }
                S2 if !spine.is_empty() => {
                    // (S f g) x -> f x (g x)
                    *fuel -= 1;
                    let app = spine.pop().unwrap();
                    let f = self.follow_mut(self.nodes[n as usize].a);
                    let g = self.follow_mut(self.nodes[n as usize].b);
                    let x = self.nodes[app as usize].b;
                    let fx = self.alloc(APP, f, x);
                    let gx = self.alloc(APP, g, x);
                    let result = self.alloc(APP, fx, gx);
                    self.save_node(app);
                    self.nodes[app as usize].tag = IND;
                    self.nodes[app as usize].a = result;
                    n = result;
                    continue;
                }
                _ => {
                    // No reduction possible - return outermost remaining node
                    if !spine.is_empty() {
                        return spine[0];
                    }
                    return n;
                }
            }
        }
    }
}

/// Parse compact string into arena.
fn parse_compact(arena: &mut Arena, input: &[u8]) -> u32 {
    // Share primitive combinator nodes across the parsed graph.
    // APP nodes still represent the full expression structure.
    let s_node = arena.alloc(S, NIL, NIL);
    let k_node = arena.alloc(K, NIL, NIL);
    let i_node = arena.alloc(I, NIL, NIL);
    let mut stack: Vec<u32> = Vec::with_capacity(1024);
    for &c in input {
        match c {
            b'k' => stack.push(s_node),
            b'X' => stack.push(k_node),
            b'D' => stack.push(i_node),
            b'-' => {
                let y = stack.pop().expect("stack underflow on '-'");
                let x = stack.pop().expect("stack underflow on '-'");
                stack.push(arena.alloc(APP, x, y));
            }
            b'\n' | b'\r' | b' ' => {} // skip whitespace
            _ => {}                    // skip unknown
        }
    }
    assert_eq!(
        stack.len(),
        1,
        "parse error: stack has {} elements",
        stack.len()
    );
    stack[0]
}

/// Build false = KI
fn make_false(arena: &mut Arena) -> u32 {
    let k = arena.alloc(K, NIL, NIL);
    let i = arena.alloc(I, NIL, NIL);
    arena.alloc(APP, k, i)
}

/// Build true = S(KK)I
fn make_true(arena: &mut Arena) -> u32 {
    let k1 = arena.alloc(K, NIL, NIL);
    let k2 = arena.alloc(K, NIL, NIL);
    let kk = arena.alloc(APP, k1, k2);
    let s = arena.alloc(S, NIL, NIL);
    let skk = arena.alloc(APP, s, kk);
    let i = arena.alloc(I, NIL, NIL);
    arena.alloc(APP, skk, i)
}

/// Build 2-arg Scott pair: pair(A, B) = S(KK)(S(SI(KA))(KB))
/// pair(f)(g) = f(A)(B) — takes 2 continuation args
fn make_pair(arena: &mut Arena, a: u32, b: u32) -> u32 {
    // inner = S(SI(KA))(KB)
    let s1 = arena.alloc(S, NIL, NIL);
    let i1 = arena.alloc(I, NIL, NIL);
    let si = arena.alloc(APP, s1, i1);
    let k_a = arena.alloc(K, NIL, NIL);
    let ka = arena.alloc(APP, k_a, a);
    let si_ka = arena.alloc(APP, si, ka);
    let s2 = arena.alloc(S, NIL, NIL);
    let s_si_ka = arena.alloc(APP, s2, si_ka);
    let k_b = arena.alloc(K, NIL, NIL);
    let kb = arena.alloc(APP, k_b, b);
    let inner = arena.alloc(APP, s_si_ka, kb);
    // outer = S(KK)(inner)
    let s3 = arena.alloc(S, NIL, NIL);
    let k3 = arena.alloc(K, NIL, NIL);
    let k4 = arena.alloc(K, NIL, NIL);
    let kk = arena.alloc(APP, k3, k4);
    let s_kk = arena.alloc(APP, s3, kk);
    arena.alloc(APP, s_kk, inner)
}

/// Decode boolean: apply to two unique markers.
fn decode_bool(arena: &mut Arena, node: u32, fuel: u64) -> Option<bool> {
    let marker_t = arena.intern_marker_t();
    let marker_f = arena.intern_marker_f();
    let app1 = arena.alloc(APP, node, marker_t);
    let app2 = arena.alloc(APP, app1, marker_f);
    let mut f = fuel;
    let result = arena.whnf(app2, &mut f);
    let result = arena.follow(result);
    let tag = arena.nodes[result as usize].tag;
    if tag == 100 {
        Some(true)
    } else if tag == 101 {
        Some(false)
    } else {
        None
    }
}

/// Decode Scott-encoded binary number.
/// Numbers are pair chains: pair(bit, rest) with 0 = pair(false, nil).
/// Each pair is a 2-arg Scott pair: pair(f)(g) = f(A)(B).
fn decode_scott_num(arena: &mut Arena, node: u32, fuel: u64) -> Option<u64> {
    let mut bits: Vec<u8> = Vec::new();
    let mut current = node;
    let mut remaining = fuel;
    // Each pair extraction/bool decode is cheap (~10-20 steps for pre-built pairs)
    let fuel_per_op = (fuel / 200).max(10000);

    for _ in 0..64 {
        if remaining < fuel_per_op * 4 {
            break;
        }

        // Extract fst (the bit) using 2-arg pair extraction
        let mut f1 = fuel_per_op;
        let first = pair_fst(arena, current, &mut f1);
        remaining = remaining.saturating_sub(fuel_per_op - f1);

        // Decode the bit
        let bit_val = decode_bool(arena, first, fuel_per_op);
        match bit_val {
            Some(false) => {
                // This could be 0-terminator pair(false, nil)
                // Check if snd is nil (false)
                let mut f2 = fuel_per_op;
                let second = pair_snd(arena, current, &mut f2);
                remaining = remaining.saturating_sub(fuel_per_op - f2);

                let snd_is_nil = decode_bool(arena, second, fuel_per_op);
                if snd_is_nil == Some(false) {
                    // pair(false, nil) → end of number (0 terminator)
                    break;
                } else {
                    // pair(false, rest) → bit 0, continue
                    bits.push(0);
                    current = second;
                }
            }
            Some(true) => {
                // pair(true, rest) → bit 1
                bits.push(1);
                let mut f2 = fuel_per_op;
                let second = pair_snd(arena, current, &mut f2);
                remaining = remaining.saturating_sub(fuel_per_op - f2);
                current = second;
            }
            None => {
                // Cannot decode as boolean - not a number
                break;
            }
        }
    }

    // 0 = pair(false, nil) → bits is empty, that's fine, return 0
    if bits.is_empty() {
        // Verify this is actually a pair by checking fst
        let mut vf = fuel_per_op;
        let fst_check = pair_fst(arena, node, &mut vf);
        let fst_bool = decode_bool(arena, fst_check, fuel_per_op);
        if fst_bool == Some(false) {
            return Some(0); // pair(false, ...) with no more bits = 0
        }
        return None; // not a number
    }
    let mut n: u64 = 0;
    for (i, &b) in bits.iter().enumerate() {
        n += (b as u64) << i;
    }
    Some(n)
}

/// Try to decode the result as a stream of bytes (list of numbers).
/// Uses 2-arg Scott pair extraction.
fn output_byte_stream(arena: &mut Arena, node: u32, fuel: u64) {
    let mut current = node;
    let mut remaining_fuel = fuel;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut count = 0u64;

    loop {
        if remaining_fuel == 0 {
            eprintln!("\n[Fuel exhausted after {} bytes]", count);
            break;
        }

        // Check if nil (end of list): decode_bool on the pair itself
        // nil = KI, which is false when applied to two args
        let is_nil = decode_bool(arena, current, remaining_fuel / 20);
        if is_nil == Some(false) {
            eprintln!("\n[End of stream after {} bytes]", count);
            break;
        }
        // Note: is_nil == None means it's a pair (not a simple boolean) - continue

        // Extract head and tail using 2-arg pair extraction
        let mut f1 = remaining_fuel / 20;
        let head = pair_fst(arena, current, &mut f1);

        let mut f2 = remaining_fuel / 20;
        let tail = pair_snd(arena, current, &mut f2);

        remaining_fuel = remaining_fuel.saturating_sub(fuel / 20 * 2);

        if let Some(n) = decode_scott_num(arena, head, remaining_fuel / 20) {
            if n < 256 {
                let _ = out.write_all(&[n as u8]);
            } else {
                eprintln!("\n[Value {} at position {} exceeds byte range]", n, count);
            }
            count += 1;
            if count % 1000 == 0 {
                let _ = out.flush();
                eprint!("\r[{} bytes output, {} nodes]", count, arena.nodes.len());
            }
        } else {
            eprintln!("\n[Failed to decode number at position {}]", count);
            break;
        }

        current = tail;
    }
    let _ = out.flush();
}

/// Describe a WHNF node for debugging.
fn describe(arena: &Arena, idx: u32, depth: usize) -> String {
    if depth > 8 {
        return "...".to_string();
    }
    let idx = arena.follow(idx);
    let n = &arena.nodes[idx as usize];
    match n.tag {
        S => "S".to_string(),
        K => "K".to_string(),
        I => "I".to_string(),
        APP => {
            let f = describe(arena, n.a, depth + 1);
            let a = describe(arena, n.b, depth + 1);
            format!("({} {})", f, a)
        }
        S1 => format!("(S {})", describe(arena, n.a, depth + 1)),
        S2 => format!(
            "(S {} {})",
            describe(arena, n.a, depth + 1),
            describe(arena, n.b, depth + 1)
        ),
        K1 => format!("(K {})", describe(arena, n.a, depth + 1)),
        _ => format!("?{}", n.tag),
    }
}

/// Decode a list of Scott-encoded numbers.
/// Uses 2-arg Scott pair extraction.
fn decode_number_list(arena: &mut Arena, node: u32, fuel: u64, max_items: usize) -> Vec<u64> {
    let mut result = Vec::new();
    let mut current = node;
    let mut remaining_fuel = fuel;

    for _ in 0..max_items {
        if remaining_fuel == 0 {
            break;
        }

        let is_nil = decode_bool(arena, current, remaining_fuel / 20);
        if is_nil == Some(false) {
            break;
        }
        // None means pair (non-nil) - continue

        let mut f1 = remaining_fuel / 20;
        let head = pair_fst(arena, current, &mut f1);

        let mut f2 = remaining_fuel / 20;
        let tail = pair_snd(arena, current, &mut f2);

        remaining_fuel = remaining_fuel.saturating_sub(fuel / 20 * 2);

        if let Some(n) = decode_scott_num(arena, head, remaining_fuel / 20) {
            result.push(n);
        } else {
            break;
        }

        current = tail;
    }
    result
}

/// Decode a list of booleans (for image pixel data).
/// Uses 2-arg Scott pair extraction.
fn decode_bool_list(arena: &mut Arena, node: u32, fuel: u64, max_items: usize) -> Vec<bool> {
    let mut result = Vec::new();
    let mut current = node;
    let mut remaining_fuel = fuel;

    for _ in 0..max_items {
        if remaining_fuel == 0 {
            break;
        }

        let is_nil = decode_bool(arena, current, remaining_fuel / 20);
        if is_nil == Some(false) {
            break;
        }
        // None means pair (non-nil) - continue

        let mut f1 = remaining_fuel / 20;
        let head = pair_fst(arena, current, &mut f1);

        let mut f2 = remaining_fuel / 20;
        let tail = pair_snd(arena, current, &mut f2);

        remaining_fuel = remaining_fuel.saturating_sub(fuel / 20 * 2);

        // Decode head as boolean
        match decode_bool(arena, head, remaining_fuel / 20) {
            Some(b) => result.push(b),
            None => break,
        }

        current = tail;
    }
    result
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: ski-eval <compact-file> [--fuel N] [--decode list|stream|bool|num|boollist|describe|io] [--io-alloc-limit N]");
        process::exit(1);
    }

    let filename = &args[1];
    let mut fuel: u64 = 100_000_000;
    let mut decode_mode = "describe".to_string();
    let mut render_var: u64 = 4;
    let mut grid_size: u64 = 0; // 0 = use render_var as grid size
    let mut img_path = "d:/github/atgt2026hp_stars/images/rendered".to_string();
    let mut key_codes: Vec<u64> = Vec::new(); // --key 5,0,17,5,3
    let mut io_alloc_limit: Option<usize> = None; // --io-alloc-limit N

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--fuel" => {
                i += 1;
                fuel = args[i].parse().expect("invalid fuel value");
            }
            "--decode" => {
                i += 1;
                decode_mode = args[i].clone();
            }
            "--var" => {
                i += 1;
                render_var = args[i].parse().expect("invalid var value");
            }
            "--img" => {
                i += 1;
                img_path = args[i].clone();
            }
            "--grid" => {
                i += 1;
                grid_size = args[i].parse().expect("invalid grid value");
            }
            "--key" => {
                i += 1;
                key_codes = args[i]
                    .split(',')
                    .map(|s| s.trim().parse::<u64>().expect("invalid key code"))
                    .collect();
                eprintln!("Key codes: {:?}", key_codes);
            }
            "--io-alloc-limit" => {
                i += 1;
                io_alloc_limit = Some(args[i].parse().expect("invalid io-alloc-limit value"));
            }
            _ => {
                eprintln!("Unknown option: {}", args[i]);
                process::exit(1);
            }
        }
        i += 1;
    }

    eprintln!("Reading {}...", filename);
    let input = fs::read(filename).expect("failed to read file");
    let input_len = input.len();
    eprintln!("  {} bytes", input_len);

    // Estimate node count
    let estimated_nodes = input_len * 2;
    let mut arena = Arena::new(estimated_nodes);

    eprintln!("Parsing...");
    let root = parse_compact(&mut arena, &input);
    eprintln!("  {} nodes", arena.nodes.len());

    eprintln!("Evaluating (fuel={})...", fuel);
    let mut remaining_fuel = fuel;
    let result = arena.whnf(root, &mut remaining_fuel);
    let steps = fuel - remaining_fuel;
    eprintln!("  {} reduction steps", steps);
    eprintln!("  {} total nodes (after reduction)", arena.nodes.len());

    let result = arena.follow(result);

    match decode_mode.as_str() {
        "describe" => {
            let desc = describe(&arena, result, 0);
            if desc.len() > 5000 {
                println!("{}", &desc[..5000]);
                println!("... (truncated)");
            } else {
                println!("{}", desc);
            }
        }
        "bool" => match decode_bool(&mut arena, result, remaining_fuel) {
            Some(true) => println!("TRUE"),
            Some(false) => println!("FALSE"),
            None => println!("NOT A BOOLEAN"),
        },
        "num" => match decode_scott_num(&mut arena, result, remaining_fuel) {
            Some(n) => println!("{}", n),
            None => println!("NOT A NUMBER"),
        },
        "list" => {
            let nums = decode_number_list(&mut arena, result, remaining_fuel, 100000);
            for n in &nums {
                print!("{} ", n);
            }
            println!();
            eprintln!("  {} items decoded", nums.len());
        }
        "boollist" => {
            let bools = decode_bool_list(&mut arena, result, remaining_fuel, 100000);
            for b in &bools {
                print!("{}", if *b { "1" } else { "0" });
            }
            println!();
            eprintln!("  {} items decoded", bools.len());
        }
        "stream" => {
            output_byte_stream(&mut arena, result, remaining_fuel);
        }
        "fst" => {
            // Extract first element of pair using 2-arg extraction
            let mut f = remaining_fuel;
            let fst = pair_fst(&mut arena, result, &mut f);
            let steps2 = remaining_fuel - f;
            eprintln!("  fst: {} additional steps", steps2);

            // Try various decodings
            let desc = describe(&arena, fst, 0);
            if desc.len() > 2000 {
                eprintln!("  describe: {}...", &desc[..2000]);
            } else {
                eprintln!("  describe: {}", desc);
            }
            match decode_bool(&mut arena, fst, f) {
                Some(true) => println!("fst = TRUE"),
                Some(false) => println!("fst = FALSE"),
                None => match decode_scott_num(&mut arena, fst, f) {
                    Some(n) => println!("fst = NUMBER({})", n),
                    None => println!(
                        "fst = {}",
                        if desc.len() > 200 {
                            &desc[..200]
                        } else {
                            &desc
                        }
                    ),
                },
            }
        }
        "snd" => {
            // Extract second element using 2-arg extraction
            let mut f = remaining_fuel;
            let snd = pair_snd(&mut arena, result, &mut f);
            let steps2 = remaining_fuel - f;
            eprintln!("  snd: {} additional steps", steps2);

            let desc = describe(&arena, snd, 0);
            if desc.len() > 2000 {
                eprintln!("  describe: {}...", &desc[..2000]);
            } else {
                eprintln!("  describe: {}", desc);
            }
            match decode_bool(&mut arena, snd, f) {
                Some(true) => println!("snd = TRUE"),
                Some(false) => println!("snd = FALSE"),
                None => match decode_scott_num(&mut arena, snd, f) {
                    Some(n) => println!("snd = NUMBER({})", n),
                    None => println!(
                        "snd = {}",
                        if desc.len() > 200 {
                            &desc[..200]
                        } else {
                            &desc
                        }
                    ),
                },
            }
        }
        "deep" => {
            // Recursively unpack pairs and try to decode structure
            eprintln!("Deep decoding...");
            deep_decode(&mut arena, result, remaining_fuel, 0, 20);
        }
        "qtree" => {
            // Interpret output and render as quadtree image
            eprintln!("Quadtree image rendering...");

            // The output is PAIR(header, image_data)
            // header = PAIR(1,1) might be format code 3 (image)
            // Extract image_data = snd(result)
            let image_data = pair_snd(&mut arena, result, &mut remaining_fuel);
            eprintln!("  Extracted image data");

            // === Method 1: Diamond encoding PAIR(cond, PAIR(qa, PAIR(qb, PAIR(qc, qd)))) ===
            for depth in &[8, 10] {
                let size = 1usize << depth;
                let mut pixels = vec![255u8; size * size]; // default white
                let mut pixel_count = 0u64;
                eprintln!("  Diamond {}x{} (depth {})...", size, size, depth);
                render_diamond(
                    &mut arena,
                    image_data,
                    &mut pixels,
                    0,
                    0,
                    size,
                    size,
                    &mut remaining_fuel,
                    &mut pixel_count,
                );
                eprintln!(
                    "    {} pixels rendered, {} nodes",
                    pixel_count,
                    arena.nodes.len()
                );
                let fname = format!(
                    "d:/github/atgt2026hp_stars/images/diamond_{}x{}.pgm",
                    size, size
                );
                write_pgm(&fname, size, size, &pixels);
                eprintln!("    Saved {}", fname);
            }

            // === Method 2: PAIR(PAIR(nw,ne), PAIR(sw,se)) with snd(result) ===
            for depth in &[8, 10] {
                let size = 1usize << depth;
                let mut pixels = vec![255u8; size * size];
                let mut pixel_count = 0u64;
                eprintln!("  Quadtree v2 {}x{} on snd(result)...", size, size);
                render_quadtree_v2(
                    &mut arena,
                    image_data,
                    &mut pixels,
                    0,
                    0,
                    size,
                    size,
                    &mut remaining_fuel,
                    &mut pixel_count,
                );
                eprintln!(
                    "    {} pixels rendered, {} nodes",
                    pixel_count,
                    arena.nodes.len()
                );
                let fname = format!(
                    "d:/github/atgt2026hp_stars/images/qtree2_snd_{}x{}.pgm",
                    size, size
                );
                write_pgm(&fname, size, size, &pixels);
                eprintln!("    Saved {}", fname);
            }

            // === Method 3: PAIR(PAIR(nw,ne), PAIR(sw,se)) with full result ===
            for depth in &[8, 10] {
                let size = 1usize << depth;
                let mut pixels = vec![255u8; size * size];
                let mut pixel_count = 0u64;
                eprintln!("  Quadtree v2 {}x{} on full result...", size, size);
                render_quadtree_v2(
                    &mut arena,
                    result,
                    &mut pixels,
                    0,
                    0,
                    size,
                    size,
                    &mut remaining_fuel,
                    &mut pixel_count,
                );
                eprintln!(
                    "    {} pixels rendered, {} nodes",
                    pixel_count,
                    arena.nodes.len()
                );
                let fname = format!(
                    "d:/github/atgt2026hp_stars/images/qtree2_full_{}x{}.pgm",
                    size, size
                );
                write_pgm(&fname, size, size, &pixels);
                eprintln!("    Saved {}", fname);
            }
        }
        "leaves" => {
            // Walk the output as a binary tree, collecting boolean leaves
            eprintln!("Collecting boolean leaves from output tree...");
            let snd_r = pair_snd(&mut arena, result, &mut remaining_fuel);
            let mut leaves: Vec<u8> = Vec::new();
            collect_bool_leaves(&mut arena, snd_r, &mut remaining_fuel, &mut leaves, 500000);
            eprintln!("  Collected {} boolean leaves", leaves.len());
            if leaves.len() > 100 {
                let sample: Vec<u8> = leaves[..100].to_vec();
                eprintln!("  First 100: {:?}", sample);
            }

            // Also from full result
            let mut leaves2: Vec<u8> = Vec::new();
            collect_bool_leaves(
                &mut arena,
                result,
                &mut remaining_fuel,
                &mut leaves2,
                500000,
            );
            eprintln!("  From full result: {} boolean leaves", leaves2.len());
            if leaves2.len() > 100 {
                let sample: Vec<u8> = leaves2[..100].to_vec();
                eprintln!("  First 100: {:?}", sample);
            }

            // Try rendering as image with width 4096
            for (name, lvs) in &[("snd", &leaves), ("full", &leaves2)] {
                let n = lvs.len();
                if n < 100 {
                    continue;
                }
                for width in &[4096usize, 2048, 1024, 512, 256, 128] {
                    if n < *width {
                        continue;
                    }
                    let height = n / width;
                    if height < 10 {
                        continue;
                    }
                    let pixels: Vec<u8> = lvs[..width * height]
                        .iter()
                        .map(|&b| if b == 1 { 0u8 } else { 255u8 })
                        .collect();
                    let fname = format!(
                        "d:/github/atgt2026hp_stars/images/leaves_{}_{}x{}.pgm",
                        name, width, height
                    );
                    write_pgm(&fname, *width, height, &pixels);
                    eprintln!("  Saved {}", fname);
                }
            }
        }
        "trace" => {
            // Trace the structure level by level
            eprintln!("Tracing output structure...");
            let mut f = remaining_fuel;

            eprintln!("\n=== Level 0: result ===");
            let desc0 = describe(&arena, result, 0);
            eprintln!(
                "  {}",
                if desc0.len() > 300 {
                    &desc0[..300]
                } else {
                    &desc0
                }
            );

            let a = pair_fst(&mut arena, result, &mut f);
            let b = pair_snd(&mut arena, result, &mut f);

            eprintln!("\n=== Level 1a: fst(result) ===");
            let da = describe(&arena, a, 0);
            eprintln!("  {}", if da.len() > 300 { &da[..300] } else { &da });
            if let Some(bn) = decode_scott_num(&mut arena, a, f.min(1000000)) {
                eprintln!("  -> NUMBER({})", bn);
            }

            eprintln!("\n=== Level 1b: snd(result) ===");
            let db = describe(&arena, b, 0);
            eprintln!("  {}", if db.len() > 300 { &db[..300] } else { &db });
            if let Some(bb) = decode_bool(&mut arena, b, f.min(1000000)) {
                eprintln!("  -> BOOL: {}", bb);
            }

            let b_fst = pair_fst(&mut arena, b, &mut f);
            let b_snd = pair_snd(&mut arena, b, &mut f);

            eprintln!("\n=== Level 2a: fst(snd(result)) ===");
            let d2a = describe(&arena, b_fst, 0);
            eprintln!("  {}", if d2a.len() > 300 { &d2a[..300] } else { &d2a });
            if let Some(bn) = decode_scott_num(&mut arena, b_fst, f.min(1000000)) {
                eprintln!("  -> NUMBER({})", bn);
            }
            if let Some(bb) = decode_bool(&mut arena, b_fst, f.min(1000000)) {
                eprintln!("  -> BOOL: {}", bb);
            }

            eprintln!("\n=== Level 2b: snd(snd(result)) ===");
            let d2b = describe(&arena, b_snd, 0);
            eprintln!("  {}", if d2b.len() > 300 { &d2b[..300] } else { &d2b });
            if let Some(bb) = decode_bool(&mut arena, b_snd, f.min(1000000)) {
                eprintln!("  -> BOOL: {}", bb);
            }

            // Go deeper
            let b_fst_fst = pair_fst(&mut arena, b_fst, &mut f);
            let b_fst_snd = pair_snd(&mut arena, b_fst, &mut f);

            eprintln!("\n=== Level 3a: fst(fst(snd(result))) ===");
            let d3a = describe(&arena, b_fst_fst, 0);
            eprintln!("  {}", if d3a.len() > 300 { &d3a[..300] } else { &d3a });
            if let Some(bn) = decode_scott_num(&mut arena, b_fst_fst, f.min(1000000)) {
                eprintln!("  -> NUMBER({})", bn);
            }
            if let Some(bb) = decode_bool(&mut arena, b_fst_fst, f.min(1000000)) {
                eprintln!("  -> BOOL: {}", bb);
            }

            eprintln!("\n=== Level 3b: snd(fst(snd(result))) ===");
            let d3b = describe(&arena, b_fst_snd, 0);
            eprintln!("  {}", if d3b.len() > 300 { &d3b[..300] } else { &d3b });
            if let Some(bn) = decode_scott_num(&mut arena, b_fst_snd, f.min(1000000)) {
                eprintln!("  -> NUMBER({})", bn);
            }
            if let Some(bb) = decode_bool(&mut arena, b_fst_snd, f.min(1000000)) {
                eprintln!("  -> BOOL: {}", bb);
            }

            // Level 4: go into b_fst_snd (which should be the next level of nesting)
            let l4_fst = pair_fst(&mut arena, b_fst_snd, &mut f);
            let l4_snd = pair_snd(&mut arena, b_fst_snd, &mut f);
            eprintln!("\n=== Level 4a: fst(snd(fst(snd(r)))) ===");
            if let Some(bn) = decode_scott_num(&mut arena, l4_fst, f.min(1000000)) {
                eprintln!("  -> NUMBER({})", bn);
            }
            if let Some(bb) = decode_bool(&mut arena, l4_fst, f.min(1000000)) {
                eprintln!("  -> BOOL: {}", bb);
            }
            let d4a = describe(&arena, l4_fst, 0);
            eprintln!("  {}", if d4a.len() > 300 { &d4a[..300] } else { &d4a });

            eprintln!("\n=== Level 4b: snd(snd(fst(snd(r)))) ===");
            if let Some(bn) = decode_scott_num(&mut arena, l4_snd, f.min(1000000)) {
                eprintln!("  -> NUMBER({})", bn);
            }
            if let Some(bb) = decode_bool(&mut arena, l4_snd, f.min(1000000)) {
                eprintln!("  -> BOOL: {}", bb);
            }
            let d4b = describe(&arena, l4_snd, 0);
            eprintln!("  {}", if d4b.len() > 300 { &d4b[..300] } else { &d4b });
        }
        "apply" => {
            // Try applying result to various argument combinations to find pixel function
            eprintln!("Probing result with various argument patterns...");
            let mut f = remaining_fuel;

            // Pattern 1: result(row)(col) - 2 args
            eprintln!("\n--- Pattern: result(m)(z) ---");
            for (m, z) in &[(0u64, 0u64), (0, 1), (1, 0), (1, 1), (2, 3)] {
                let mn = make_scott_num(&mut arena, *m);
                let zn = make_scott_num(&mut arena, *z);
                let app1 = arena.alloc(APP, result, mn);
                let app2 = arena.alloc(APP, app1, zn);
                let mut fuel = f.min(5000000);
                arena.whnf(app2, &mut fuel);
                let r = arena.follow(app2);
                let b = decode_bool(&mut arena, r, 1000000);
                let desc = describe(&arena, r, 0);
                let d = if desc.len() > 100 {
                    &desc[..100]
                } else {
                    &desc
                };
                eprintln!("  result({},{}) = {} bool={:?}", m, z, d, b);
            }

            // Pattern 2: result(N)(m)(z) - 3 args
            eprintln!("\n--- Pattern: result(N)(m)(z) ---");
            for (n, m, z) in &[
                (8u64, 0u64, 0u64),
                (8, 0, 1),
                (8, 1, 0),
                (8, 1, 1),
                (8, 3, 5),
            ] {
                let nn = make_scott_num(&mut arena, *n);
                let mn = make_scott_num(&mut arena, *m);
                let zn = make_scott_num(&mut arena, *z);
                let app1 = arena.alloc(APP, result, nn);
                let app2 = arena.alloc(APP, app1, mn);
                let app3 = arena.alloc(APP, app2, zn);
                let mut fuel = f.min(10000000);
                arena.whnf(app3, &mut fuel);
                let r = arena.follow(app3);
                let b = decode_bool(&mut arena, r, 1000000);
                let desc = describe(&arena, r, 0);
                let d = if desc.len() > 100 {
                    &desc[..100]
                } else {
                    &desc
                };
                eprintln!("  result({},{},{}) = {} bool={:?}", n, m, z, d, b);
            }

            // Pattern 3: result(var)(m)(z) with var=1 (initial call)
            eprintln!("\n--- Pattern: result(1)(m)(z) ---");
            for (m, z) in &[(0u64, 0u64), (0, 1), (1, 0), (1, 1)] {
                let v1 = make_scott_num(&mut arena, 1);
                let mn = make_scott_num(&mut arena, *m);
                let zn = make_scott_num(&mut arena, *z);
                let app1 = arena.alloc(APP, result, v1);
                let app2 = arena.alloc(APP, app1, mn);
                let app3 = arena.alloc(APP, app2, zn);
                let mut fuel = f.min(10000000);
                arena.whnf(app3, &mut fuel);
                let r = arena.follow(app3);
                let b = decode_bool(&mut arena, r, 1000000);
                let desc = describe(&arena, r, 0);
                let d = if desc.len() > 100 {
                    &desc[..100]
                } else {
                    &desc
                };
                eprintln!("  result(1,{},{}) = {} bool={:?}", m, z, d, b);
            }

            // Pattern 4: snd(result)(args)
            let snd_r = pair_snd(&mut arena, result, &mut f);
            eprintln!("\n--- Pattern: snd(result)(m)(z) ---");
            for (m, z) in &[(0u64, 0u64), (0, 1), (1, 0), (1, 1)] {
                let mn = make_scott_num(&mut arena, *m);
                let zn = make_scott_num(&mut arena, *z);
                let app1 = arena.alloc(APP, snd_r, mn);
                let app2 = arena.alloc(APP, app1, zn);
                let mut fuel = f.min(10000000);
                arena.whnf(app2, &mut fuel);
                let r = arena.follow(app2);
                let b = decode_bool(&mut arena, r, 1000000);
                let desc = describe(&arena, r, 0);
                let d = if desc.len() > 100 {
                    &desc[..100]
                } else {
                    &desc
                };
                eprintln!("  snd(r)({},{}) = {} bool={:?}", m, z, d, b);
            }

            // Pattern 5: fst(snd(result))(args) - maybe the actual function is deeper
            let fst_snd = pair_fst(&mut arena, snd_r, &mut f);
            eprintln!("\n--- Pattern: fst(snd(result))(m)(z) ---");
            for (m, z) in &[(0u64, 0u64), (0, 1), (1, 0), (1, 1)] {
                let mn = make_scott_num(&mut arena, *m);
                let zn = make_scott_num(&mut arena, *z);
                let app1 = arena.alloc(APP, fst_snd, mn);
                let app2 = arena.alloc(APP, app1, zn);
                let mut fuel = f.min(10000000);
                arena.whnf(app2, &mut fuel);
                let r = arena.follow(app2);
                let b = decode_bool(&mut arena, r, 1000000);
                let desc = describe(&arena, r, 0);
                let d = if desc.len() > 100 {
                    &desc[..100]
                } else {
                    &desc
                };
                eprintln!("  fst(snd(r))({},{}) = {} bool={:?}", m, z, d, b);
            }
        }
        "render" => {
            // Render image: result(N)(m)(z) -> pixel value (binary 0/1)
            // N = resolution (image is NxN pixels), m = row, z = column
            let size = render_var; // N = image size directly (not 2^var)
            eprintln!("Rendering {}x{} image (N={})...", size, size, size);

            let mut pixels = vec![128u8; (size * size) as usize]; // default gray
                                                                  // let mut decoded_count = 0u64;
            let mut bool_count = 0u64;
            let mut num_count = 0u64;
            let mut fail_count = 0u64;
            let fuel_per_pixel: u64 = 10_000_000;
            let mut fail_examples: Vec<(u64, u64, String)> = Vec::new();

            for m in 0..size {
                for z in 0..size {
                    let var_n = make_scott_num(&mut arena, size);
                    let m_n = make_scott_num(&mut arena, m);
                    let z_n = make_scott_num(&mut arena, z);
                    let app1 = arena.alloc(APP, result, var_n);
                    let app2 = arena.alloc(APP, app1, m_n);
                    let app3 = arena.alloc(APP, app2, z_n);
                    let mut pf = fuel_per_pixel;
                    arena.whnf(app3, &mut pf);
                    let r = arena.follow(app3);

                    // Try decode as Scott number, unwrapping K/S(KK) wrappers if needed
                    let mut val = r;
                    // Unwrap K(x) -> x: K1 node or APP(K, x) pattern
                    // Also unwrap S(KK)(g) -> g: S2 node where a = KK
                    for _ in 0..10 {
                        let vv = arena.follow(val);
                        let n = arena.nodes[vv as usize];
                        if n.tag == K1 {
                            val = arena.follow_mut(n.a);
                            continue;
                        }
                        if n.tag == APP {
                            let func = arena.follow(n.a);
                            if arena.nodes[func as usize].tag == K {
                                val = arena.follow_mut(n.b);
                                continue;
                            }
                        }
                        // S2(f, g) where f = KK → S(KK)(g) = K∘g → extract g
                        if n.tag == S2 {
                            let f = arena.follow(n.a);
                            let fn_node = arena.nodes[f as usize];
                            // Check if f = K1(K) i.e. KK
                            if fn_node.tag == K1 {
                                let inner = arena.follow(fn_node.a);
                                if arena.nodes[inner as usize].tag == K {
                                    val = arena.follow_mut(n.b);
                                    continue;
                                }
                            }
                            // Also check if f = APP(K, K)
                            if fn_node.tag == APP {
                                let fa = arena.follow(fn_node.a);
                                let fb = arena.follow(fn_node.b);
                                if arena.nodes[fa as usize].tag == K
                                    && arena.nodes[fb as usize].tag == K
                                {
                                    val = arena.follow_mut(n.b);
                                    continue;
                                }
                            }
                        }
                        break;
                    }

                    if let Some(n) = decode_scott_num(&mut arena, val, 1_000_000) {
                        pixels[(m * size + z) as usize] = (n.min(255)) as u8;
                        num_count += 1;
                        // decoded_count += 1;
                    } else if let Some(b) = decode_bool(&mut arena, val, 500_000) {
                        pixels[(m * size + z) as usize] = if b { 255 } else { 0 };
                        bool_count += 1;
                        // decoded_count += 1;
                    } else {
                        fail_count += 1;
                        if fail_examples.len() < 5 {
                            let desc = describe(&arena, val, 0);
                            let d = if desc.len() > 200 {
                                desc[..200].to_string()
                            } else {
                                desc
                            };
                            fail_examples.push((m, z, d));
                        }
                    }
                }
                if (m + 1) % 4 == 0 || m == size - 1 {
                    eprint!(
                        "\r  row {}/{} ({} num, {} bool, {} fail, {} nodes)     ",
                        m + 1,
                        size,
                        num_count,
                        bool_count,
                        fail_count,
                        arena.nodes.len()
                    );
                }
            }
            eprintln!();
            eprintln!(
                "  Decoded: {} num, {} bool, {} fail out of {}",
                num_count,
                bool_count,
                fail_count,
                size * size
            );

            // Find max value for normalization
            let max_val = pixels
                .iter()
                .copied()
                .filter(|&p| p != 128)
                .max()
                .unwrap_or(1);
            eprintln!("  Max pixel value: {}", max_val);

            // Save raw image
            let fname = format!("{}_{}x{}.pgm", img_path, size, size);
            write_pgm(&fname, size as usize, size as usize, &pixels);
            eprintln!("  Saved {}", fname);

            // Also save normalized version if max > 1
            if max_val > 1 && max_val < 255 {
                let normalized: Vec<u8> = pixels
                    .iter()
                    .map(|&p| {
                        if p == 128 {
                            128
                        }
                        // keep gray for unknown
                        else {
                            ((p as u32) * 255 / max_val as u32).min(255) as u8
                        }
                    })
                    .collect();
                let fname2 = format!("{}_norm_{}x{}.pgm", img_path, size, size);
                write_pgm(&fname2, size as usize, size as usize, &normalized);
                eprintln!("  Saved {}", fname2);
            }

            // Print fail examples
            if !fail_examples.is_empty() {
                eprintln!("  Failed pixel examples:");
                for (m, z, desc) in &fail_examples {
                    eprintln!("    ({},{}) = {}", m, z, desc);
                }
            }

            // Print sample pixel values (numeric)
            let sample = 16.min(size);
            eprintln!("  Sample pixel values (first {}x{}):", sample, sample);
            for m in 0..sample {
                eprint!("    ");
                for z in 0..sample {
                    let p = pixels[(m * size + z) as usize];
                    eprint!("{:3} ", p);
                }
                eprintln!();
            }
            // Also print as visual
            eprintln!("  Visual (0=. else #):");
            for m in 0..sample {
                eprint!("    ");
                for z in 0..sample {
                    let p = pixels[(m * size + z) as usize];
                    if p == 128 {
                        eprint!("? ");
                    } else if p == 0 {
                        eprint!(". ");
                    } else {
                        eprint!("# ");
                    }
                }
                eprintln!();
            }
        }
        "render2" => {
            // Render with N and render_size decoupled.
            // N (the format wrapper arg) = render_var
            // render_size = the actual pixel grid dimension
            // This lets us use N=4 or N=16 (which work) but render at higher resolution.
            let n_arg = render_var;
            let render_size = if grid_size > 0 { grid_size } else { n_arg };

            eprintln!(
                "Rendering {}x{} image using N={} as format arg...",
                render_size, render_size, n_arg
            );

            let mut pixels = vec![128u8; (render_size * render_size) as usize];
            let mut num_count = 0u64;
            let mut bool_count = 0u64;
            let mut fail_count = 0u64;
            let fuel_per_pixel: u64 = 10_000_000;
            let mut fail_examples: Vec<(u64, u64, String)> = Vec::new();

            for m in 0..render_size {
                for z in 0..render_size {
                    let var_n = make_scott_num(&mut arena, n_arg); // N = fixed
                    let m_n = make_scott_num(&mut arena, m);
                    let z_n = make_scott_num(&mut arena, z);
                    let app1 = arena.alloc(APP, result, var_n);
                    let app2 = arena.alloc(APP, app1, m_n);
                    let app3 = arena.alloc(APP, app2, z_n);
                    let mut pf = fuel_per_pixel;
                    arena.whnf(app3, &mut pf);
                    let r = arena.follow(app3);

                    // Try decode as Scott number
                    let mut val = r;
                    for _ in 0..10 {
                        let vv = arena.follow(val);
                        let n = arena.nodes[vv as usize];
                        if n.tag == K1 {
                            val = arena.follow_mut(n.a);
                            continue;
                        }
                        if n.tag == APP {
                            let func = arena.follow(n.a);
                            if arena.nodes[func as usize].tag == K {
                                val = arena.follow_mut(n.b);
                                continue;
                            }
                        }
                        if n.tag == S2 {
                            let f = arena.follow(n.a);
                            let fn_node = arena.nodes[f as usize];
                            if fn_node.tag == K1 {
                                let inner = arena.follow(fn_node.a);
                                if arena.nodes[inner as usize].tag == K {
                                    val = arena.follow_mut(n.b);
                                    continue;
                                }
                            }
                            if fn_node.tag == APP {
                                let fa = arena.follow(fn_node.a);
                                let fb = arena.follow(fn_node.b);
                                if arena.nodes[fa as usize].tag == K
                                    && arena.nodes[fb as usize].tag == K
                                {
                                    val = arena.follow_mut(n.b);
                                    continue;
                                }
                            }
                        }
                        break;
                    }

                    if let Some(n) = decode_scott_num(&mut arena, val, 1_000_000) {
                        pixels[(m * render_size + z) as usize] = (n.min(255)) as u8;
                        num_count += 1;
                    } else if let Some(b) = decode_bool(&mut arena, val, 500_000) {
                        pixels[(m * render_size + z) as usize] = if b { 255 } else { 0 };
                        bool_count += 1;
                    } else {
                        fail_count += 1;
                        if fail_examples.len() < 5 {
                            let desc = describe(&arena, val, 0);
                            let d = if desc.len() > 200 {
                                desc[..200].to_string()
                            } else {
                                desc
                            };
                            fail_examples.push((m, z, d));
                        }
                    }
                }
                if (m + 1) % 4 == 0 || m == render_size - 1 {
                    eprint!(
                        "\r  row {}/{} ({} num, {} bool, {} fail, {} nodes)     ",
                        m + 1,
                        render_size,
                        num_count,
                        bool_count,
                        fail_count,
                        arena.nodes.len()
                    );
                }
            }
            eprintln!();
            eprintln!(
                "  Decoded: {} num, {} bool, {} fail out of {}",
                num_count,
                bool_count,
                fail_count,
                render_size * render_size
            );

            let max_val = pixels
                .iter()
                .copied()
                .filter(|&p| p != 128)
                .max()
                .unwrap_or(1);
            eprintln!("  Max pixel value: {}", max_val);

            let fname = format!(
                "{}_N{}_{}x{}.pgm",
                img_path, n_arg, render_size, render_size
            );
            write_pgm(&fname, render_size as usize, render_size as usize, &pixels);
            eprintln!("  Saved {}", fname);

            if max_val > 1 && max_val < 255 {
                let normalized: Vec<u8> = pixels
                    .iter()
                    .map(|&p| {
                        if p == 128 {
                            128
                        } else {
                            ((p as u32) * 255 / max_val as u32).min(255) as u8
                        }
                    })
                    .collect();
                let fname2 = format!(
                    "{}_N{}_norm_{}x{}.pgm",
                    img_path, n_arg, render_size, render_size
                );
                write_pgm(
                    &fname2,
                    render_size as usize,
                    render_size as usize,
                    &normalized,
                );
                eprintln!("  Saved {}", fname2);
            }

            if !fail_examples.is_empty() {
                eprintln!("  Failed pixel examples:");
                for (m, z, desc) in &fail_examples {
                    eprintln!("    ({},{}) = {}", m, z, desc);
                }
            }

            // Print sample pixel values
            let sample = 16.min(render_size);
            eprintln!("  Sample pixel values (first {}x{}):", sample, sample);
            for m in 0..sample {
                eprint!("    ");
                for z in 0..sample {
                    let p = pixels[(m * render_size + z) as usize];
                    eprint!("{:3} ", p);
                }
                eprintln!();
            }
        }
        "structure" => {
            // Deep examination of result = S(SI(KA))(Y) structure
            eprintln!("Examining result structure...");
            let r = arena.follow(result);
            let rn = arena.nodes[r as usize];
            eprintln!(
                "result: tag={} ({})",
                rn.tag,
                match rn.tag {
                    0 => "APP",
                    1 => "S",
                    2 => "K",
                    3 => "I",
                    4 => "S1",
                    5 => "S2",
                    6 => "K1",
                    7 => "IND",
                    _ => "?",
                }
            );

            if rn.tag == S2 {
                // result = S2(f, Y)
                let f = arena.follow(rn.a);
                let y = arena.follow(rn.b);
                let fn_node = arena.nodes[f as usize];
                let yn = arena.nodes[y as usize];
                eprintln!("  f (result.a): tag={}", fn_node.tag);
                eprintln!("  Y (result.b): tag={}", yn.tag);

                // f should be S2(I, K(A))
                if fn_node.tag == S2 {
                    let f_a = arena.follow(fn_node.a); // should be I
                    let f_b = arena.follow(fn_node.b); // should be K1(A)
                    eprintln!("  f.a: tag={}", arena.nodes[f_a as usize].tag);
                    eprintln!("  f.b: tag={}", arena.nodes[f_b as usize].tag);

                    // f.b = K1(A)
                    let fb_node = arena.nodes[f_b as usize];
                    if fb_node.tag == K1 {
                        let a_node_idx = arena.follow(fb_node.a); // A
                        let a_node = arena.nodes[a_node_idx as usize];
                        eprintln!("  A: tag={}", a_node.tag);
                        let a_desc = describe(&arena, a_node_idx, 3);
                        eprintln!("  A describe(depth=3): {}", a_desc);

                        // If A = S2(SI(K(p1)), K(p2)) = pair(p1, p2)
                        if a_node.tag == S2 {
                            let a_a = arena.follow(a_node.a); // SI(K(p1))
                            let a_b = arena.follow(a_node.b); // K(p2)
                            eprintln!("  A.a = {}", describe(&arena, a_a, 3));
                            eprintln!("  A.b = {}", describe(&arena, a_b, 3));

                            // Extract p2 from K(p2) = K1(p2)
                            let ab_node = arena.nodes[a_b as usize];
                            if ab_node.tag == K1 {
                                let p2 = arena.follow(ab_node.a);
                                eprintln!("  p2 = {}", describe(&arena, p2, 5));
                                if let Some(n) = decode_scott_num(&mut arena, p2, 1_000_000) {
                                    eprintln!("  p2 = NUMBER({})", n);
                                }
                            }

                            // Extract p1 from SI(K(p1)) = S2(I, K(p1))
                            let aa_node = arena.nodes[a_a as usize];
                            if aa_node.tag == S2 {
                                let aa_b = arena.follow(aa_node.b); // K(p1) = K1(p1)
                                let aab_node = arena.nodes[aa_b as usize];
                                if aab_node.tag == K1 {
                                    let p1 = arena.follow(aab_node.a);
                                    eprintln!("  p1 = {}", describe(&arena, p1, 5));
                                    if let Some(n) = decode_scott_num(&mut arena, p1, 1_000_000) {
                                        eprintln!("  p1 = NUMBER({})", n);
                                    }
                                }
                            }
                        }
                    }
                }

                // Describe Y briefly
                let y_desc = describe(&arena, y, 2);
                eprintln!("  Y describe(depth=2): {}", y_desc);

                // Try Y as a pair
                let mut f = remaining_fuel;
                let y_fst = pair_fst(&mut arena, y, &mut f);
                let yf_desc = describe(&arena, y_fst, 2);
                eprintln!("  fst(Y) = {}", yf_desc);

                let y_snd = pair_snd(&mut arena, y, &mut f);
                let ys_desc = describe(&arena, y_snd, 2);
                eprintln!("  snd(Y) = {}", ys_desc);
            } else if rn.tag == APP {
                let la = arena.follow(rn.a);
                let lb = arena.follow(rn.b);
                eprintln!(
                    "  APP: left tag={} right tag={}",
                    arena.nodes[la as usize].tag, arena.nodes[lb as usize].tag
                );

                // Navigate deeper if left is also APP
                let la_node = arena.nodes[la as usize];
                if la_node.tag == APP {
                    let lla = arena.follow(la_node.a);
                    let llb = arena.follow(la_node.b);
                    eprintln!(
                        "  Left is APP(tag={}, tag={})",
                        arena.nodes[lla as usize].tag, arena.nodes[llb as usize].tag
                    );
                    if arena.nodes[lla as usize].tag == S {
                        eprintln!("  → result = S(f)(Y) where f and Y are:");
                        eprintln!("  f = {}", describe(&arena, llb, 3));
                        eprintln!("  Y = {}", describe(&arena, lb, 2));
                    }
                }
            }
        }
        "examine" => {
            // Examine result(N) structure
            let n = make_scott_num(&mut arena, render_var);
            let app = arena.alloc(APP, result, n);
            let mut f = remaining_fuel;
            arena.whnf(app, &mut f);
            let rn = arena.follow(app);
            let desc = describe(&arena, rn, 0);
            eprintln!("result({}) WHNF:", render_var);
            eprintln!(
                "  {}",
                if desc.len() > 3000 {
                    &desc[..3000]
                } else {
                    &desc
                }
            );

            // Try as number list
            let nums = decode_number_list(&mut arena, rn, f.min(10_000_000), 200);
            if !nums.is_empty() {
                eprintln!(
                    "As number list ({} items): {:?}",
                    nums.len(),
                    &nums[..nums.len().min(50)]
                );
            }

            // Try as bool list
            let bools = decode_bool_list(&mut arena, rn, f.min(10_000_000), 200);
            if !bools.is_empty() {
                eprintln!(
                    "As bool list ({} items): {:?}",
                    bools.len(),
                    &bools[..bools.len().min(50)]
                );
            }

            // Try fst
            let fst_val = pair_fst(&mut arena, rn, &mut f);
            let fst_desc = describe(&arena, fst_val, 0);
            eprintln!(
                "fst(result({})): {}",
                render_var,
                if fst_desc.len() > 1000 {
                    &fst_desc[..1000]
                } else {
                    &fst_desc
                }
            );
            if let Some(n) = decode_scott_num(&mut arena, fst_val, f.min(5_000_000)) {
                eprintln!("  = NUMBER({})", n);
            }
            if let Some(b) = decode_bool(&mut arena, fst_val, f.min(5_000_000)) {
                eprintln!("  = BOOL({})", b);
            }

            // Try snd
            let snd_val = pair_snd(&mut arena, rn, &mut f);
            let snd_desc = describe(&arena, snd_val, 0);
            eprintln!(
                "snd(result({})): {}",
                render_var,
                if snd_desc.len() > 1000 {
                    &snd_desc[..1000]
                } else {
                    &snd_desc
                }
            );
            if let Some(n) = decode_scott_num(&mut arena, snd_val, f.min(5_000_000)) {
                eprintln!("  = NUMBER({})", n);
            }
            if let Some(b) = decode_bool(&mut arena, snd_val, f.min(5_000_000)) {
                eprintln!("  = BOOL({})", b);
            }

            // Try result(N)(0)
            let zero = make_scott_num(&mut arena, 0);
            let app_zero = arena.alloc(APP, rn, zero);
            let mut f2 = f.min(50_000_000);
            arena.whnf(app_zero, &mut f2);
            let r0 = arena.follow(app_zero);
            let r0_desc = describe(&arena, r0, 0);
            eprintln!(
                "result({})(0): {}",
                render_var,
                if r0_desc.len() > 1000 {
                    &r0_desc[..1000]
                } else {
                    &r0_desc
                }
            );

            // Try result(N)(0)(0)
            let zero2 = make_scott_num(&mut arena, 0);
            let app_00 = arena.alloc(APP, r0, zero2);
            let mut f3 = f.min(50_000_000);
            arena.whnf(app_00, &mut f3);
            let r00 = arena.follow(app_00);
            let r00_desc = describe(&arena, r00, 0);
            eprintln!(
                "result({})(0)(0): {}",
                render_var,
                if r00_desc.len() > 500 {
                    &r00_desc[..500]
                } else {
                    &r00_desc
                }
            );
            if let Some(n) = decode_scott_num(&mut arena, r00, f.min(5_000_000)) {
                eprintln!("  = NUMBER({})", n);
            }

            // Try result(N) as stream (output bytes)
            eprintln!("\nTrying result({}) as byte stream...", render_var);
            let rn2 = arena.follow(app); // re-follow
            output_byte_stream(&mut arena, rn2, f.min(50_000_000));
        }
        "nsweep" => {
            // Quick sweep of N values to find which produce meaningful images
            // Tests a few pixels per N to distinguish meaningful vs gradient
            eprintln!("Sweeping N values to find meaningful images...");
            let n_start = if render_var > 2 { render_var } else { 2 };
            let n_end = if grid_size > 0 { grid_size } else { 512 };

            let test_coords: Vec<(u64, u64)> = vec![
                (0, 0),
                (0, 1),
                (1, 0),
                (1, 1),
                (0, 2),
                (2, 0),
                (2, 1),
                (1, 2),
            ];

            for n in n_start..=n_end {
                let mut pixel_vals: Vec<(u64, u64, String)> = Vec::new();
                // let mut all_ok = true;

                for &(m, z) in &test_coords {
                    if m >= n || z >= n {
                        continue;
                    }
                    let var_n = make_scott_num(&mut arena, n);
                    let m_n = make_scott_num(&mut arena, m);
                    let z_n = make_scott_num(&mut arena, z);
                    let app1 = arena.alloc(APP, result, var_n);
                    let app2 = arena.alloc(APP, app1, m_n);
                    let app3 = arena.alloc(APP, app2, z_n);
                    let mut pf: u64 = 5_000_000;
                    arena.whnf(app3, &mut pf);
                    let r = arena.follow(app3);

                    // Unwrap K wrappers
                    let mut val = r;
                    for _ in 0..10 {
                        let vv = arena.follow(val);
                        let nn = arena.nodes[vv as usize];
                        if nn.tag == K1 {
                            val = arena.follow_mut(nn.a);
                            continue;
                        }
                        if nn.tag == APP {
                            let func = arena.follow(nn.a);
                            if arena.nodes[func as usize].tag == K {
                                val = arena.follow_mut(nn.b);
                                continue;
                            }
                        }
                        break;
                    }

                    if let Some(num) = decode_scott_num(&mut arena, val, 500_000) {
                        pixel_vals.push((m, z, format!("{}", num)));
                    } else if let Some(b) = decode_bool(&mut arena, val, 200_000) {
                        pixel_vals.push((m, z, format!("{}", if b { "T" } else { "F" })));
                    } else {
                        let desc = describe(&arena, val, 0);
                        let d = if desc.len() > 60 {
                            desc[..60].to_string()
                        } else {
                            desc
                        };
                        pixel_vals.push((m, z, format!("?{}", d)));
                        // all_ok = false;
                    }
                }

                // Check if row 0 and row 1 differ (gradient test)
                let r0c1 = pixel_vals
                    .iter()
                    .find(|(m, z, _)| *m == 0 && *z == 1)
                    .map(|(_, _, v)| v.as_str())
                    .unwrap_or("");
                let r1c1 = pixel_vals
                    .iter()
                    .find(|(m, z, _)| *m == 1 && *z == 1)
                    .map(|(_, _, v)| v.as_str())
                    .unwrap_or("");
                let r0c0 = pixel_vals
                    .iter()
                    .find(|(m, z, _)| *m == 0 && *z == 0)
                    .map(|(_, _, v)| v.as_str())
                    .unwrap_or("");
                let r1c0 = pixel_vals
                    .iter()
                    .find(|(m, z, _)| *m == 1 && *z == 0)
                    .map(|(_, _, v)| v.as_str())
                    .unwrap_or("");
                let r2c0 = pixel_vals
                    .iter()
                    .find(|(m, z, _)| *m == 2 && *z == 0)
                    .map(|(_, _, v)| v.as_str())
                    .unwrap_or("");
                let r2c1 = pixel_vals
                    .iter()
                    .find(|(m, z, _)| *m == 2 && *z == 1)
                    .map(|(_, _, v)| v.as_str())
                    .unwrap_or("");

                let rows_differ = r0c1 != r1c1 || r0c0 != r1c0;
                let marker = if rows_differ { "OK" } else { "GRAD?" };

                eprintln!(
                    "  N={:4}: [{}] (0,0)={} (0,1)={} (1,0)={} (1,1)={} (2,0)={} (2,1)={} nodes={}",
                    n,
                    marker,
                    r0c0,
                    r0c1,
                    r1c0,
                    r1c1,
                    r2c0,
                    r2c1,
                    arena.nodes.len()
                );
            }
        }
        "probe3" => {
            // Systematic probe: try result(a)(b)(c) for small values
            eprintln!("Probing result(a)(b)(c) systematically...");
            let f = remaining_fuel;

            // Test with var=4 (16x16), small coords
            eprintln!("\n--- result(var)(m)(z) with var=4 ---");
            for m in 0..4u64 {
                for z in 0..4u64 {
                    let var_n = make_scott_num(&mut arena, 4);
                    let m_n = make_scott_num(&mut arena, m);
                    let z_n = make_scott_num(&mut arena, z);
                    let app1 = arena.alloc(APP, result, var_n);
                    let app2 = arena.alloc(APP, app1, m_n);
                    let app3 = arena.alloc(APP, app2, z_n);
                    let mut pf = f.min(5_000_000);
                    arena.whnf(app3, &mut pf);
                    let r = arena.follow(app3);

                    let num = decode_scott_num(&mut arena, r, 1_000_000);
                    let b = if num.is_none() {
                        decode_bool(&mut arena, r, 500_000)
                    } else {
                        None
                    };
                    eprint!("  ({},{})=", m, z);
                    if let Some(n) = num {
                        eprint!("N{}", n);
                    } else if let Some(b) = b {
                        eprint!("B{}", b as u8);
                    } else {
                        eprint!("??");
                    }
                }
                eprintln!();
            }

            // Also test with var=1 (2x2)
            eprintln!("\n--- result(var)(m)(z) with var=1 ---");
            for m in 0..2u64 {
                for z in 0..2u64 {
                    let var_n = make_scott_num(&mut arena, 1);
                    let m_n = make_scott_num(&mut arena, m);
                    let z_n = make_scott_num(&mut arena, z);
                    let app1 = arena.alloc(APP, result, var_n);
                    let app2 = arena.alloc(APP, app1, m_n);
                    let app3 = arena.alloc(APP, app2, z_n);
                    let mut pf = f.min(10_000_000);
                    arena.whnf(app3, &mut pf);
                    let r = arena.follow(app3);
                    let num = decode_scott_num(&mut arena, r, 1_000_000);
                    let b = if num.is_none() {
                        decode_bool(&mut arena, r, 500_000)
                    } else {
                        None
                    };
                    eprint!("  ({},{})=", m, z);
                    if let Some(n) = num {
                        eprint!("N{}", n);
                    } else if let Some(b) = b {
                        eprint!("B{}", b as u8);
                    } else {
                        let desc = describe(&arena, r, 0);
                        let d = if desc.len() > 60 { &desc[..60] } else { &desc };
                        eprint!("[{}]", d);
                    }
                }
                eprintln!();
            }

            // Also try: what if we DON'T give var? result(m)(z)
            eprintln!("\n--- result(m)(z) with 2 args ---");
            for m in 0..4u64 {
                for z in 0..4u64 {
                    let m_n = make_scott_num(&mut arena, m);
                    let z_n = make_scott_num(&mut arena, z);
                    let app1 = arena.alloc(APP, result, m_n);
                    let app2 = arena.alloc(APP, app1, z_n);
                    let mut pf = f.min(5_000_000);
                    arena.whnf(app2, &mut pf);
                    let r = arena.follow(app2);
                    let num = decode_scott_num(&mut arena, r, 1_000_000);
                    let b = if num.is_none() {
                        decode_bool(&mut arena, r, 500_000)
                    } else {
                        None
                    };
                    eprint!("  ({},{})=", m, z);
                    if let Some(n) = num {
                        eprint!("N{}", n);
                    } else if let Some(b) = b {
                        eprint!("B{}", b as u8);
                    } else {
                        eprint!("??");
                    }
                }
                eprintln!();
            }
        }
        "extract" => {
            // Extract EXPR from format(output.image(EXPR)(end))
            // Theory: result = \z. z(A)(B(z))
            //   where A = fst(result) = pipeline ITEM
            //   and B = format(end) (rest of pipeline)
            //
            // For format(end) from l.4:
            //   format(end) = \z. z(pair(FALSE,FALSE))(FALSE)
            //
            // So: fst(result) = A = ITEM containing EXPR
            //     snd(result) = B(KI) should = FALSE (end marker)

            let mut f = remaining_fuel;

            // Step 1: Get A = fst(result)
            eprintln!("\n=== Step 1: A = fst(result) ===");
            let a = pair_fst(&mut arena, result, &mut f);
            let desc_a = describe(&arena, a, 0);
            eprintln!(
                "  A = {}",
                if desc_a.len() > 500 {
                    &desc_a[..500]
                } else {
                    &desc_a
                }
            );

            // Step 1b: Verify snd(result) = FALSE (end marker)
            eprintln!("\n=== Step 1b: snd(result) - should be FALSE ===");
            let b_ki = pair_snd(&mut arena, result, &mut f);
            match decode_bool(&mut arena, b_ki, f.min(1_000_000)) {
                Some(true) => eprintln!("  snd(result) = TRUE"),
                Some(false) => eprintln!("  snd(result) = FALSE  ✓ (end marker)"),
                None => {
                    let desc = describe(&arena, b_ki, 0);
                    eprintln!(
                        "  snd(result) = NOT BOOL: {}",
                        if desc.len() > 200 {
                            &desc[..200]
                        } else {
                            &desc
                        }
                    );
                }
            }

            // Step 2: Decompose A = pair(TAG, DATA)?
            eprintln!("\n=== Step 2: Decompose A ===");
            let a_fst = pair_fst(&mut arena, a, &mut f);
            let a_snd = pair_snd(&mut arena, a, &mut f);

            eprintln!("  fst(A) =");
            match decode_bool(&mut arena, a_fst, f.min(1_000_000)) {
                Some(b) => eprintln!("    BOOL({})", b),
                None => match decode_scott_num(&mut arena, a_fst, f.min(1_000_000)) {
                    Some(n) => eprintln!("    NUMBER({})", n),
                    None => {
                        let desc = describe(&arena, a_fst, 0);
                        eprintln!(
                            "    {}",
                            if desc.len() > 300 {
                                &desc[..300]
                            } else {
                                &desc
                            }
                        );
                    }
                },
            }

            eprintln!("  snd(A) =");
            match decode_bool(&mut arena, a_snd, f.min(1_000_000)) {
                Some(b) => eprintln!("    BOOL({})", b),
                None => match decode_scott_num(&mut arena, a_snd, f.min(1_000_000)) {
                    Some(n) => eprintln!("    NUMBER({})", n),
                    None => {
                        let desc = describe(&arena, a_snd, 0);
                        eprintln!(
                            "    {}",
                            if desc.len() > 300 {
                                &desc[..300]
                            } else {
                                &desc
                            }
                        );
                    }
                },
            }

            // Step 3: Go deeper - decompose fst(A) and snd(A)
            eprintln!("\n=== Step 3: Deeper decomposition ===");

            // fst(fst(A))
            let aa_fst = pair_fst(&mut arena, a_fst, &mut f);
            eprintln!("  fst(fst(A)) =");
            match decode_bool(&mut arena, aa_fst, f.min(1_000_000)) {
                Some(b) => eprintln!("    BOOL({})", b),
                None => match decode_scott_num(&mut arena, aa_fst, f.min(1_000_000)) {
                    Some(n) => eprintln!("    NUMBER({})", n),
                    None => {
                        let desc = describe(&arena, aa_fst, 0);
                        eprintln!(
                            "    {}",
                            if desc.len() > 200 {
                                &desc[..200]
                            } else {
                                &desc
                            }
                        );
                    }
                },
            }

            // snd(fst(A))
            let aa_snd = pair_snd(&mut arena, a_fst, &mut f);
            eprintln!("  snd(fst(A)) =");
            match decode_bool(&mut arena, aa_snd, f.min(1_000_000)) {
                Some(b) => eprintln!("    BOOL({})", b),
                None => match decode_scott_num(&mut arena, aa_snd, f.min(1_000_000)) {
                    Some(n) => eprintln!("    NUMBER({})", n),
                    None => {
                        let desc = describe(&arena, aa_snd, 0);
                        eprintln!(
                            "    {}",
                            if desc.len() > 200 {
                                &desc[..200]
                            } else {
                                &desc
                            }
                        );
                    }
                },
            }

            // fst(snd(A))
            let ab_fst = pair_fst(&mut arena, a_snd, &mut f);
            eprintln!("  fst(snd(A)) =");
            match decode_bool(&mut arena, ab_fst, f.min(1_000_000)) {
                Some(b) => eprintln!("    BOOL({})", b),
                None => match decode_scott_num(&mut arena, ab_fst, f.min(1_000_000)) {
                    Some(n) => eprintln!("    NUMBER({})", n),
                    None => {
                        let desc = describe(&arena, ab_fst, 0);
                        eprintln!(
                            "    {}",
                            if desc.len() > 200 {
                                &desc[..200]
                            } else {
                                &desc
                            }
                        );
                    }
                },
            }

            // snd(snd(A))
            let ab_snd = pair_snd(&mut arena, a_snd, &mut f);
            eprintln!("  snd(snd(A)) =");
            match decode_bool(&mut arena, ab_snd, f.min(1_000_000)) {
                Some(b) => eprintln!("    BOOL({})", b),
                None => match decode_scott_num(&mut arena, ab_snd, f.min(1_000_000)) {
                    Some(n) => eprintln!("    NUMBER({})", n),
                    None => {
                        let desc = describe(&arena, ab_snd, 0);
                        eprintln!(
                            "    {}",
                            if desc.len() > 200 {
                                &desc[..200]
                            } else {
                                &desc
                            }
                        );
                    }
                },
            }

            // Step 4: Try various candidates as EXPR - call with (1)(0)(0) and check for diamond
            eprintln!("\n=== Step 4: Try calling candidates as EXPR(1)(0)(0) ===");
            let candidates: Vec<(&str, u32)> = vec![
                ("A=fst(result)", a),
                ("snd(A)", a_snd),
                ("fst(A)", a_fst),
                ("fst(fst(A))", aa_fst),
                ("snd(fst(A))", aa_snd),
                ("fst(snd(A))", ab_fst),
                ("snd(snd(A))", ab_snd),
            ];

            for (name, node) in &candidates {
                let one = make_scott_num(&mut arena, 1);
                let zero1 = make_scott_num(&mut arena, 0);
                let zero2 = make_scott_num(&mut arena, 0);
                let app1 = arena.alloc(APP, *node, one);
                let app2 = arena.alloc(APP, app1, zero1);
                let app3 = arena.alloc(APP, app2, zero2);
                let mut pf = f.min(50_000_000);
                arena.whnf(app3, &mut pf);
                let r = arena.follow(app3);

                let b = decode_bool(&mut arena, r, 1_000_000);
                let desc = describe(&arena, r, 0);
                let d = if desc.len() > 200 {
                    &desc[..200]
                } else {
                    &desc
                };
                eprintln!("  {}(1)(0)(0) = {} bool={:?}", name, d, b);

                // If it's a non-boolean, check if it has diamond structure
                if b.is_none() {
                    // Try 5-element: fst = cond, snd = pair(qa, pair(qb, pair(qc, qd)))
                    let cond = pair_fst(&mut arena, r, &mut f);
                    let cond_bool = decode_bool(&mut arena, cond, 500_000);
                    let rest = pair_snd(&mut arena, r, &mut f);
                    let qa = pair_fst(&mut arena, rest, &mut f);
                    let qa_bool = decode_bool(&mut arena, qa, 500_000);
                    eprintln!(
                        "    diamond? cond={:?} qa_is_pair={}",
                        cond_bool,
                        qa_bool.is_none()
                    );
                }
            }

            // Step 5: Also try result(I) directly
            eprintln!("\n=== Step 5: result(I) ===");
            let i_node = arena.alloc(I, NIL, NIL);
            let ri = arena.alloc(APP, result, i_node);
            let mut pf = f.min(50_000_000);
            arena.whnf(ri, &mut pf);
            let ri_val = arena.follow(ri);
            let desc_ri = describe(&arena, ri_val, 0);
            eprintln!(
                "  result(I) = {}",
                if desc_ri.len() > 500 {
                    &desc_ri[..500]
                } else {
                    &desc_ri
                }
            );

            // Try result(I)(1)(0)(0)
            let one = make_scott_num(&mut arena, 1);
            let zero1 = make_scott_num(&mut arena, 0);
            let zero2 = make_scott_num(&mut arena, 0);
            let app1 = arena.alloc(APP, ri_val, one);
            let app2 = arena.alloc(APP, app1, zero1);
            let app3 = arena.alloc(APP, app2, zero2);
            let mut pf = f.min(50_000_000);
            arena.whnf(app3, &mut pf);
            let r = arena.follow(app3);
            let b = decode_bool(&mut arena, r, 1_000_000);
            let desc = describe(&arena, r, 0);
            let d = if desc.len() > 200 {
                &desc[..200]
            } else {
                &desc
            };
            eprintln!("  result(I)(1)(0)(0) = {} bool={:?}", d, b);

            // Step 6: Try passing a handler that captures the second arg
            // If format = \pipeline. pipeline(\tag. \data. data), extract data
            eprintln!("\n=== Step 6: Pass extractor handlers ===");

            // Handler: \x.\y. y (select second)
            let ki_handler = make_false(&mut arena);
            let rh = arena.alloc(APP, result, ki_handler);
            let mut pf = f.min(50_000_000);
            arena.whnf(rh, &mut pf);
            let rh_val = arena.follow(rh);
            let desc_rh = describe(&arena, rh_val, 0);
            eprintln!(
                "  result(KI) = {}",
                if desc_rh.len() > 300 {
                    &desc_rh[..300]
                } else {
                    &desc_rh
                }
            );
            let rh_bool = decode_bool(&mut arena, rh_val, 1_000_000);
            eprintln!("    bool={:?}", rh_bool);

            // Handler: \x.\y. x (select first) = K
            let k_handler = arena.alloc(K, NIL, NIL);
            let rk = arena.alloc(APP, result, k_handler);
            let mut pf = f.min(50_000_000);
            arena.whnf(rk, &mut pf);
            let rk_val = arena.follow(rk);
            let desc_rk = describe(&arena, rk_val, 0);
            eprintln!(
                "  result(K) = {}",
                if desc_rk.len() > 300 {
                    &desc_rk[..300]
                } else {
                    &desc_rk
                }
            );

            // Try calling result(K)(1)(0)(0) - maybe result(K) IS the EXPR?
            let one = make_scott_num(&mut arena, 1);
            let zero1 = make_scott_num(&mut arena, 0);
            let zero2 = make_scott_num(&mut arena, 0);
            let app1 = arena.alloc(APP, rk_val, one);
            let app2 = arena.alloc(APP, app1, zero1);
            let app3 = arena.alloc(APP, app2, zero2);
            let mut pf = f.min(50_000_000);
            arena.whnf(app3, &mut pf);
            let r = arena.follow(app3);
            let b_val = decode_bool(&mut arena, r, 1_000_000);
            let desc = describe(&arena, r, 0);
            let d = if desc.len() > 200 {
                &desc[..200]
            } else {
                &desc
            };
            eprintln!("  result(K)(1)(0)(0) = {} bool={:?}", d, b_val);

            // Step 7: Extract B from S2 node directly
            // result = S2(f, B) where result(z) = f(z)(B(z))
            // f = SI(K TAG), B = the pipeline body function
            eprintln!("\n=== Step 7: Extract B from S2 node ===");
            let r = arena.follow(result);
            let rn = arena.nodes[r as usize];
            eprintln!("  result tag = {}", rn.tag);
            if rn.tag == S2 {
                let f_part = arena.follow(rn.a);
                let b_part = arena.follow(rn.b);
                eprintln!("  f = {}", {
                    let d = describe(&arena, f_part, 0);
                    if d.len() > 200 {
                        d[..200].to_string()
                    } else {
                        d
                    }
                });
                eprintln!("  B = {}", {
                    let d = describe(&arena, b_part, 0);
                    if d.len() > 200 {
                        d[..200].to_string()
                    } else {
                        d
                    }
                });

                // B(K) = fst of the next pipeline level
                let k_node = arena.alloc(K, NIL, NIL);
                let bk = arena.alloc(APP, b_part, k_node);
                let mut pf = f.min(50_000_000);
                arena.whnf(bk, &mut pf);
                let bk_val = arena.follow(bk);
                eprintln!("  B(K) =");
                match decode_bool(&mut arena, bk_val, 1_000_000) {
                    Some(b) => eprintln!("    BOOL({})", b),
                    None => match decode_scott_num(&mut arena, bk_val, 1_000_000) {
                        Some(n) => eprintln!("    NUMBER({})", n),
                        None => {
                            let d = describe(&arena, bk_val, 0);
                            eprintln!("    {}", if d.len() > 500 { &d[..500] } else { &d });
                        }
                    },
                }

                // B(KI) = snd of the next pipeline level
                let ki_node = make_false(&mut arena);
                let bki = arena.alloc(APP, b_part, ki_node);
                let mut pf = f.min(50_000_000);
                arena.whnf(bki, &mut pf);
                let bki_val = arena.follow(bki);
                eprintln!("  B(KI) =");
                match decode_bool(&mut arena, bki_val, 1_000_000) {
                    Some(b) => eprintln!("    BOOL({})", b),
                    None => match decode_scott_num(&mut arena, bki_val, 1_000_000) {
                        Some(n) => eprintln!("    NUMBER({})", n),
                        None => {
                            let d = describe(&arena, bki_val, 0);
                            eprintln!("    {}", if d.len() > 500 { &d[..500] } else { &d });
                        }
                    },
                }

                // Now check if B is ALSO an S2 node (nested pipeline)
                let bn = arena.nodes[arena.follow(b_part) as usize];
                eprintln!("  B tag = {}", bn.tag);
                if bn.tag == S2 {
                    let b_f = arena.follow(bn.a);
                    let b_b = arena.follow(bn.b); // This is C, the next level
                    eprintln!("  B.f = {}", {
                        let d = describe(&arena, b_f, 0);
                        if d.len() > 200 {
                            d[..200].to_string()
                        } else {
                            d
                        }
                    });
                    eprintln!("  B.B (=C) = {}", {
                        let d = describe(&arena, b_b, 0);
                        if d.len() > 200 {
                            d[..200].to_string()
                        } else {
                            d
                        }
                    });

                    // C(K) = fst of NEXT next level
                    let k2 = arena.alloc(K, NIL, NIL);
                    let ck = arena.alloc(APP, b_b, k2);
                    let mut pf = f.min(50_000_000);
                    arena.whnf(ck, &mut pf);
                    let ck_val = arena.follow(ck);
                    eprintln!("  C(K) =");
                    match decode_bool(&mut arena, ck_val, 1_000_000) {
                        Some(b) => eprintln!("    BOOL({})", b),
                        None => match decode_scott_num(&mut arena, ck_val, 1_000_000) {
                            Some(n) => eprintln!("    NUMBER({})", n),
                            None => {
                                let d = describe(&arena, ck_val, 0);
                                eprintln!("    {}", if d.len() > 500 { &d[..500] } else { &d });
                            }
                        },
                    }

                    // C(KI) = snd
                    let ki2 = make_false(&mut arena);
                    let cki = arena.alloc(APP, b_b, ki2);
                    let mut pf = f.min(50_000_000);
                    arena.whnf(cki, &mut pf);
                    let cki_val = arena.follow(cki);
                    eprintln!("  C(KI) =");
                    match decode_bool(&mut arena, cki_val, 1_000_000) {
                        Some(b) => eprintln!("    BOOL({})", b),
                        None => {
                            let d = describe(&arena, cki_val, 0);
                            eprintln!("    {}", if d.len() > 200 { &d[..200] } else { &d });
                        }
                    }
                }

                // Step 8: Try B(K) as EXPR - call with (1)(0)(0)
                eprintln!("\n=== Step 8: Try B(K) as EXPR(1)(0)(0) ===");
                let one = make_scott_num(&mut arena, 1);
                let zero1 = make_scott_num(&mut arena, 0);
                let zero2 = make_scott_num(&mut arena, 0);
                let app1 = arena.alloc(APP, bk_val, one);
                let app2 = arena.alloc(APP, app1, zero1);
                let app3 = arena.alloc(APP, app2, zero2);
                let mut pf = f.min(100_000_000);
                arena.whnf(app3, &mut pf);
                let r = arena.follow(app3);
                let b_check = decode_bool(&mut arena, r, 1_000_000);
                let desc = describe(&arena, r, 0);
                let d = if desc.len() > 500 {
                    &desc[..500]
                } else {
                    &desc
                };
                eprintln!("  B(K)(1)(0)(0) = {} bool={:?}", d, b_check);
                // Check diamond structure
                if b_check.is_none() {
                    let cond = pair_fst(&mut arena, r, &mut f);
                    let cb = decode_bool(&mut arena, cond, 500_000);
                    eprintln!("    fst (cond?) = {:?}", cb);
                    let rest = pair_snd(&mut arena, r, &mut f);
                    let rb = decode_bool(&mut arena, rest, 500_000);
                    eprintln!("    snd = bool={:?}", rb);
                    if rb.is_none() {
                        let qa = pair_fst(&mut arena, rest, &mut f);
                        let qa_b = decode_bool(&mut arena, qa, 500_000);
                        eprintln!("    fst(snd) (qa?) = bool={:?}", qa_b);
                    }
                }

                // Step 9: Try B(K)(K) as EXPR(K) to see structure
                eprintln!("\n=== Step 9: Explore B(K) structure ===");
                let bk_fst = pair_fst(&mut arena, bk_val, &mut f);
                let bk_snd = pair_snd(&mut arena, bk_val, &mut f);
                eprintln!("  fst(B(K)) =");
                match decode_bool(&mut arena, bk_fst, 1_000_000) {
                    Some(b) => eprintln!("    BOOL({})", b),
                    None => match decode_scott_num(&mut arena, bk_fst, 1_000_000) {
                        Some(n) => eprintln!("    NUMBER({})", n),
                        None => {
                            let d = describe(&arena, bk_fst, 0);
                            eprintln!("    {}", if d.len() > 300 { &d[..300] } else { &d });
                        }
                    },
                }
                eprintln!("  snd(B(K)) =");
                match decode_bool(&mut arena, bk_snd, 1_000_000) {
                    Some(b) => eprintln!("    BOOL({})", b),
                    None => match decode_scott_num(&mut arena, bk_snd, 1_000_000) {
                        Some(n) => eprintln!("    NUMBER({})", n),
                        None => {
                            let d = describe(&arena, bk_snd, 0);
                            eprintln!("    {}", if d.len() > 300 { &d[..300] } else { &d });
                        }
                    },
                }
            } else {
                eprintln!("  Result is NOT S2 - tag = {}", rn.tag);
            }
        }
        "payload" => {
            // Extract payload from format wrapper and render as diamond tree.
            // result = S2(f, K1(payload)). The payload is the image EXPR thunk.
            // When forced, it should produce a diamond quadtree structure.
            eprintln!("Extracting payload from format wrapper...");

            let r = arena.follow(result);
            let rn = arena.nodes[r as usize];

            // result can be:
            //   S2(f, K1(payload)) - after being applied once
            //   APP(APP(S, f), K1(payload)) - before application (WHNF form)
            //   APP(S1(f), K1(payload)) - intermediate
            let b_part_opt: Option<u32> = if rn.tag == S2 {
                Some(arena.follow(rn.b))
            } else if rn.tag == APP {
                // result = APP(something, B). B is the second component.
                let b_raw = arena.follow(rn.b);
                eprintln!(
                    "  result is APP: .a tag={}, .b tag={}",
                    arena.nodes[arena.follow(rn.a) as usize].tag,
                    arena.nodes[b_raw as usize].tag
                );
                Some(b_raw)
            } else {
                None
            };

            if b_part_opt.is_none() {
                eprintln!("ERROR: result has unexpected tag={}", rn.tag);
            } else {
                // Y = result.b — the image encoder function
                // result = S(f)(Y) where f = SI(K·type_tag)
                // result(N) = f(N)(Y(N)) = N(type_tag)(Y(N))
                // For N where Scott encoding passes through: result(N) = Y(N)
                // Y(N) should be the diamond tree for resolution N
                let y_func = b_part_opt.unwrap();
                let yn = arena.nodes[y_func as usize];
                eprintln!("  Y tag = {}", yn.tag);
                let yd = describe(&arena, y_func, 0);
                eprintln!("  Y = {}", if yd.len() > 300 { &yd[..300] } else { &yd });

                // Apply Y to various N values and examine
                for n_val in [2u64, 4, 8, 16] {
                    let n_scott = make_scott_num(&mut arena, n_val);
                    let app = arena.alloc(APP, y_func, n_scott);
                    let mut pf = remaining_fuel.min(100_000_000);
                    arena.whnf(app, &mut pf);
                    let yn_result = arena.follow(app);
                    let fuel_used = 100_000_000u64.min(remaining_fuel) - pf;

                    let is_bool = decode_bool(&mut arena, yn_result, 1_000_000);
                    let tag = arena.nodes[yn_result as usize].tag;
                    eprintln!("\n  Y({}) tag={}, fuel_used={}", n_val, tag, fuel_used);
                    if let Some(b) = is_bool {
                        eprintln!("    = BOOL({})", b);
                    } else {
                        let d = describe(&arena, yn_result, 0);
                        eprintln!("    = {}", if d.len() > 300 { &d[..300] } else { &d });

                        // Check diamond structure: pair(cond, pair(qa, pair(qb, pair(qc, qd))))
                        let cond = pair_fst(&mut arena, yn_result, &mut remaining_fuel);
                        let cond_bool = decode_bool(&mut arena, cond, 1_000_000);
                        eprintln!("    cond = {:?}", cond_bool);

                        if cond_bool.is_some() {
                            // It's a diamond! Try rendering
                            eprintln!("    → Valid diamond root! Rendering...");
                            let size = (n_val as usize).next_power_of_two().max(16);
                            let mut pixels = vec![255u8; size * size];
                            let mut pixel_count = 0u64;
                            let mut rf = remaining_fuel.min(500_000_000);
                            render_diamond(
                                &mut arena,
                                yn_result,
                                &mut pixels,
                                0,
                                0,
                                size,
                                size,
                                &mut rf,
                                &mut pixel_count,
                            );
                            let black = pixels.iter().filter(|&&p| p == 0).count();
                            let white = pixels.iter().filter(|&&p| p == 255).count();
                            let gray = pixels.iter().filter(|&&p| p == 128).count();
                            eprintln!(
                                "    {}x{}: {} pix rendered, black={}, white={}, gray={}",
                                size, size, pixel_count, black, white, gray
                            );
                            let fname =
                                format!("{}_Yn{}_diamond_{}x{}.pgm", img_path, n_val, size, size);
                            write_pgm(&fname, size, size, &pixels);
                            eprintln!("    Saved {}", fname);
                        }
                    }
                }

                // Also try: extract Y(N) for N=4 and render as diamond at various sizes
                eprintln!("\n  === Rendering Y(4) as diamond at multiple sizes ===");
                let n_scott_4 = make_scott_num(&mut arena, 4);
                let y4_app = arena.alloc(APP, y_func, n_scott_4);
                let mut pf_y4 = remaining_fuel.min(100_000_000);
                arena.whnf(y4_app, &mut pf_y4);
                let y4 = arena.follow(y4_app);
                eprintln!("  Y(4) tag={}", arena.nodes[y4 as usize].tag);

                for depth in &[4usize, 6, 8, 10] {
                    let size = 1usize << depth;
                    let mut pixels = vec![255u8; size * size];
                    let mut pixel_count = 0u64;
                    let mut rf = remaining_fuel.min(500_000_000);
                    eprintln!("  Diamond {}x{} from Y(4)...", size, size);
                    render_diamond(
                        &mut arena,
                        y4,
                        &mut pixels,
                        0,
                        0,
                        size,
                        size,
                        &mut rf,
                        &mut pixel_count,
                    );
                    let black = pixels.iter().filter(|&&p| p == 0).count();
                    let white = pixels.iter().filter(|&&p| p == 255).count();
                    let gray = pixels.iter().filter(|&&p| p == 128).count();
                    eprintln!(
                        "    {} pix, black={}, white={}, gray={}, nodes={}",
                        pixel_count,
                        black,
                        white,
                        gray,
                        arena.nodes.len()
                    );
                    let fname = format!("{}_Y4_diamond_{}x{}.pgm", img_path, size, size);
                    write_pgm(&fname, size, size, &pixels);
                    eprintln!("    Saved {}", fname);
                }

                // And Y(16) at higher res
                eprintln!("\n  === Rendering Y(16) as diamond at multiple sizes ===");
                let n_scott_16 = make_scott_num(&mut arena, 16);
                let y16_app = arena.alloc(APP, y_func, n_scott_16);
                let mut pf_y16 = remaining_fuel.min(100_000_000);
                arena.whnf(y16_app, &mut pf_y16);
                let y16 = arena.follow(y16_app);
                eprintln!("  Y(16) tag={}", arena.nodes[y16 as usize].tag);

                for depth in &[4usize, 6, 8, 10] {
                    let size = 1usize << depth;
                    let mut pixels = vec![255u8; size * size];
                    let mut pixel_count = 0u64;
                    let mut rf = remaining_fuel.min(500_000_000);
                    eprintln!("  Diamond {}x{} from Y(16)...", size, size);
                    render_diamond(
                        &mut arena,
                        y16,
                        &mut pixels,
                        0,
                        0,
                        size,
                        size,
                        &mut rf,
                        &mut pixel_count,
                    );
                    let black = pixels.iter().filter(|&&p| p == 0).count();
                    let white = pixels.iter().filter(|&&p| p == 255).count();
                    let gray = pixels.iter().filter(|&&p| p == 128).count();
                    eprintln!(
                        "    {} pix, black={}, white={}, gray={}, nodes={}",
                        pixel_count,
                        black,
                        white,
                        gray,
                        arena.nodes.len()
                    );
                    let fname = format!("{}_Y16_diamond_{}x{}.pgm", img_path, size, size);
                    write_pgm(&fname, size, size, &pixels);
                    eprintln!("    Saved {}", fname);
                }

                // Also try full result(N) for N=4,16 and render as diamond
                eprintln!("\n  === Rendering result(4) and result(16) as diamond ===");
                for n_val in [4u64, 16] {
                    let n_scott = make_scott_num(&mut arena, n_val);
                    let rn_app = arena.alloc(APP, result, n_scott);
                    let mut pf_rn = remaining_fuel.min(100_000_000);
                    arena.whnf(rn_app, &mut pf_rn);
                    let rn_result = arena.follow(rn_app);
                    let rn_tag = arena.nodes[rn_result as usize].tag;
                    eprintln!("  result({}) tag={}", n_val, rn_tag);

                    let is_bool = decode_bool(&mut arena, rn_result, 1_000_000);
                    if let Some(b) = is_bool {
                        eprintln!("    = BOOL({})", b);
                    } else {
                        // Try as diamond tree
                        let cond = pair_fst(&mut arena, rn_result, &mut remaining_fuel);
                        let cond_bool = decode_bool(&mut arena, cond, 1_000_000);
                        eprintln!("    cond = {:?}", cond_bool);

                        let size = 256usize;
                        let mut pixels = vec![255u8; size * size];
                        let mut pixel_count = 0u64;
                        let mut rf = remaining_fuel.min(500_000_000);
                        eprintln!("    Diamond {}x{} from result({})...", size, size, n_val);
                        render_diamond(
                            &mut arena,
                            rn_result,
                            &mut pixels,
                            0,
                            0,
                            size,
                            size,
                            &mut rf,
                            &mut pixel_count,
                        );
                        let black = pixels.iter().filter(|&&p| p == 0).count();
                        let white = pixels.iter().filter(|&&p| p == 255).count();
                        let gray = pixels.iter().filter(|&&p| p == 128).count();
                        eprintln!(
                            "    {} pix, black={}, white={}, gray={}",
                            pixel_count, black, white, gray
                        );
                        let fname =
                            format!("{}_result{}_diamond_{}x{}.pgm", img_path, n_val, size, size);
                        write_pgm(&fname, size, size, &pixels);
                        eprintln!("    Saved {}", fname);
                    }
                }
            }
        }
        "ntest" => {
            // Quick test: for N=1..64, try result(N)(0)(1) and check if it decodes
            eprintln!("Testing which N values work for result(N)(0)(1)...");
            for n in 1..=64u64 {
                let var_n = make_scott_num(&mut arena, n);
                let m_n = make_scott_num(&mut arena, 0);
                let z_n = make_scott_num(&mut arena, 1);
                let app1 = arena.alloc(APP, result, var_n);
                let app2 = arena.alloc(APP, app1, m_n);
                let app3 = arena.alloc(APP, app2, z_n);
                let mut pf = remaining_fuel.min(10_000_000);
                arena.whnf(app3, &mut pf);
                let r = arena.follow(app3);

                // Try unwrapping
                let mut val = r;
                for _ in 0..10 {
                    let vv = arena.follow(val);
                    let nd = arena.nodes[vv as usize];
                    if nd.tag == K1 {
                        val = arena.follow_mut(nd.a);
                        continue;
                    }
                    if nd.tag == APP {
                        let func = arena.follow(nd.a);
                        if arena.nodes[func as usize].tag == K {
                            val = arena.follow_mut(nd.b);
                            continue;
                        }
                    }
                    break;
                }

                let num = decode_scott_num(&mut arena, val, 1_000_000);
                let b = if num.is_none() {
                    decode_bool(&mut arena, val, 500_000)
                } else {
                    None
                };
                let tag = arena.nodes[arena.follow(val) as usize].tag;
                if let Some(n_val) = num {
                    eprint!("N={:3}: num={:<5}  ", n, n_val);
                } else if let Some(bv) = b {
                    eprint!("N={:3}: bool={}   ", n, bv);
                } else {
                    eprint!("N={:3}: FAIL t={}  ", n, tag);
                }
                if n % 4 == 0 {
                    eprintln!();
                }
            }
            eprintln!();
        }
        "selftest" => {
            // Self-test: verify pair encoding, number encoding, and extraction
            eprintln!("=== Self-test: pair/number encoding ===\n");
            let mut ok = true;
            let test_fuel: u64 = 1_000_000;

            // Test 1: true(x)(y) = x
            {
                let t = make_true(&mut arena);
                let marker_x = arena.alloc(100, NIL, NIL);
                let marker_y = arena.alloc(101, NIL, NIL);
                let app1 = arena.alloc(APP, t, marker_x);
                let app2 = arena.alloc(APP, app1, marker_y);
                let mut f = test_fuel;
                arena.whnf(app2, &mut f);
                let r = arena.follow(app2);
                if arena.nodes[r as usize].tag == 100 {
                    eprintln!("  [OK] true(x)(y) = x");
                } else {
                    eprintln!(
                        "  [FAIL] true(x)(y) = tag {}, expected 100",
                        arena.nodes[r as usize].tag
                    );
                    ok = false;
                }
            }

            // Test 2: false(x)(y) = y
            {
                let f_node = make_false(&mut arena);
                let marker_x = arena.alloc(100, NIL, NIL);
                let marker_y = arena.alloc(101, NIL, NIL);
                let app1 = arena.alloc(APP, f_node, marker_x);
                let app2 = arena.alloc(APP, app1, marker_y);
                let mut f = test_fuel;
                arena.whnf(app2, &mut f);
                let r = arena.follow(app2);
                if arena.nodes[r as usize].tag == 101 {
                    eprintln!("  [OK] false(x)(y) = y");
                } else {
                    eprintln!(
                        "  [FAIL] false(x)(y) = tag {}, expected 101",
                        arena.nodes[r as usize].tag
                    );
                    ok = false;
                }
            }

            // Test 3: pair(a,b)(K)(dummy) = a
            {
                let marker_a = arena.alloc(100, NIL, NIL);
                let marker_b = arena.alloc(101, NIL, NIL);
                let p = make_pair(&mut arena, marker_a, marker_b);
                let mut f = test_fuel;
                let fst = pair_fst(&mut arena, p, &mut f);
                if arena.nodes[fst as usize].tag == 100 {
                    eprintln!("  [OK] fst(pair(a,b)) = a");
                } else {
                    eprintln!(
                        "  [FAIL] fst(pair(a,b)) = tag {}, expected 100",
                        arena.nodes[fst as usize].tag
                    );
                    ok = false;
                }
            }

            // Test 4: pair(a,b)(KI)(dummy) = b
            {
                let marker_a = arena.alloc(100, NIL, NIL);
                let marker_b = arena.alloc(101, NIL, NIL);
                let p = make_pair(&mut arena, marker_a, marker_b);
                let mut f = test_fuel;
                let snd = pair_snd(&mut arena, p, &mut f);
                if arena.nodes[snd as usize].tag == 101 {
                    eprintln!("  [OK] snd(pair(a,b)) = b");
                } else {
                    eprintln!(
                        "  [FAIL] snd(pair(a,b)) = tag {}, expected 101",
                        arena.nodes[snd as usize].tag
                    );
                    ok = false;
                }
            }

            // Test 5: decode_bool on true/false
            {
                let t = make_true(&mut arena);
                let b = decode_bool(&mut arena, t, test_fuel);
                if b == Some(true) {
                    eprintln!("  [OK] decode_bool(true) = true");
                } else {
                    eprintln!("  [FAIL] decode_bool(true) = {:?}", b);
                    ok = false;
                }
                let f = make_false(&mut arena);
                let b2 = decode_bool(&mut arena, f, test_fuel);
                if b2 == Some(false) {
                    eprintln!("  [OK] decode_bool(false) = false");
                } else {
                    eprintln!("  [FAIL] decode_bool(false) = {:?}", b2);
                    ok = false;
                }
            }

            // Test 6: encode/decode numbers 0..15
            eprintln!();
            for n in 0..=15u64 {
                let num_node = make_scott_num(&mut arena, n);
                let decoded = decode_scott_num(&mut arena, num_node, test_fuel);
                if decoded == Some(n) {
                    eprint!("  [OK] num({})={} ", n, n);
                } else {
                    eprint!("  [FAIL] num({})={:?} ", n, decoded);
                    ok = false;
                }
                if (n + 1) % 8 == 0 {
                    eprintln!();
                }
            }

            // Test 7: Verify number 3 compact matches server-verified encoding
            {
                let expected =
                    "kXX--kkD-XkXX--D----XkXX--kkD-XkXX--D----XkXX--kkD-XXD----XXD----------";
                let n3_from_compact = parse_compact(&mut arena, expected.as_bytes());
                let dec = decode_scott_num(&mut arena, n3_from_compact, test_fuel);
                if dec == Some(3) {
                    eprintln!("  [OK] Server-verified compact of 3 decodes to 3");
                } else {
                    eprintln!("  [FAIL] Server-verified compact of 3 decodes to {:?}", dec);
                    ok = false;
                }
            }

            // Test 8: Verify large numbers
            for n in &[100u64, 255, 1000, 65535] {
                let num_node = make_scott_num(&mut arena, *n);
                let decoded = decode_scott_num(&mut arena, num_node, 10_000_000);
                if decoded == Some(*n) {
                    eprintln!("  [OK] num({}) round-trips", n);
                } else {
                    eprintln!("  [FAIL] num({}) decoded as {:?}", n, decoded);
                    ok = false;
                }
            }

            eprintln!();
            if ok {
                eprintln!("=== All self-tests PASSED ===");
            } else {
                eprintln!("=== Some self-tests FAILED ===");
            }
        }
        "walk1" => {
            // Walk the output structure using 1-arg pair extraction.
            // The program output uses 1-arg Scott pairs:
            //   pair1(A, B) = S(SI(KA))(KB)
            //   pair1(A, B)(handler) = handler(A)(B)
            // Extract: node(K) = A, node(KI) = B
            eprintln!("Walking output with 1-arg pair extraction...");
            let mut f = remaining_fuel;
            let mut current = result;

            for i in 0..20 {
                eprintln!("\n=== List item {} ===", i);

                // Check if current is a boolean (nil/end marker)
                let is_bool = decode_bool(&mut arena, current, f.min(1_000_000));
                if let Some(b) = is_bool {
                    eprintln!("  = BOOL({}) → end of list", b);
                    break;
                }

                // Extract head (1-arg)
                let head = pair1_fst(&mut arena, current, &mut f);
                let tail = pair1_snd(&mut arena, current, &mut f);

                // Describe head
                let desc = describe(&arena, head, 0);
                eprintln!(
                    "  head = {}",
                    if desc.len() > 300 {
                        &desc[..300]
                    } else {
                        &desc
                    }
                );

                // Try decode head as various types
                if let Some(b) = decode_bool(&mut arena, head, f.min(1_000_000)) {
                    eprintln!("  head = BOOL({})", b);
                } else if let Some(n) = decode_scott_num(&mut arena, head, f.min(1_000_000)) {
                    eprintln!("  head = NUMBER({})", n);
                } else {
                    // Head might be a 1-arg pair (nested structure)
                    let h_fst = pair1_fst(&mut arena, head, &mut f);
                    let h_snd = pair1_snd(&mut arena, head, &mut f);
                    let hf_bool = decode_bool(&mut arena, h_fst, f.min(500_000));
                    let hf_num = if hf_bool.is_none() {
                        decode_scott_num(&mut arena, h_fst, f.min(500_000))
                    } else {
                        None
                    };
                    let hs_bool = decode_bool(&mut arena, h_snd, f.min(500_000));
                    let hs_num = if hs_bool.is_none() {
                        decode_scott_num(&mut arena, h_snd, f.min(500_000))
                    } else {
                        None
                    };
                    eprintln!("  head.fst = bool={:?} num={:?}", hf_bool, hf_num);
                    eprintln!("  head.snd = bool={:?} num={:?}", hs_bool, hs_num);

                    // If head is a 5-element (5-argument image symbol):
                    // head(handler) = handler(a1)(a2)(a3)(a4)(a5)
                    // Try extracting 5 fields by applying a 5-arg extractor
                    eprintln!("  Trying head as 5-arg constructor...");
                    for arg_idx in 0..5 {
                        // Build extractor: λa1..a5. a_{arg_idx+1}
                        // For arg 0: K(K(K(K)))  → gets 1st (K⁴)
                        // For arg 1: K(K(K(KI))) → gets 2nd ...
                        // Actually: extractors for 5-arg:
                        // arg0: λa.λb.λc.λd.λe. a  = difficult in pure SKI
                        // Instead, apply head to a sequence of K/KI extractors
                        // head(K)(K)(K)(K)(K) gets us: K(a1)(a2)(a3)(a4)(a5) = a1(a3)(a4)(a5)
                        // That's wrong. We need Church-style extractors.

                        // Simpler: just apply head to 5 unique markers and see what comes out
                        let markers: Vec<u32> =
                            (0..5).map(|j| arena.alloc(110 + j, NIL, NIL)).collect();
                        let mut app = arena.alloc(APP, head, markers[0]);
                        for &m in &markers[1..] {
                            app = arena.alloc(APP, app, m);
                        }
                        let mut pf = f.min(10_000_000);
                        arena.whnf(app, &mut pf);
                        let r = arena.follow(app);
                        let tag = arena.nodes[r as usize].tag;
                        let r_desc = describe(&arena, r, 0);
                        let rd = if r_desc.len() > 100 {
                            &r_desc[..100]
                        } else {
                            &r_desc
                        };
                        eprintln!("    head(m0)(m1)(m2)(m3)(m4) tag={} = {}", tag, rd);
                        break; // only need to test once
                    }

                    // Also try: head as 1-arg pair and walk deeper
                    eprintln!("  Walking head as 1-arg pair list:");
                    let mut hcur = head;
                    for j in 0..8 {
                        let hb = decode_bool(&mut arena, hcur, f.min(500_000));
                        if let Some(b) = hb {
                            eprintln!("    [{}] BOOL({})", j, b);
                            break;
                        }
                        let hn = decode_scott_num(&mut arena, hcur, f.min(500_000));
                        if let Some(n) = hn {
                            eprintln!("    [{}] NUMBER({})", j, n);
                            break;
                        }
                        let hh = pair1_fst(&mut arena, hcur, &mut f);
                        let ht = pair1_snd(&mut arena, hcur, &mut f);
                        let hh_b = decode_bool(&mut arena, hh, f.min(500_000));
                        let hh_n = if hh_b.is_none() {
                            decode_scott_num(&mut arena, hh, f.min(500_000))
                        } else {
                            None
                        };
                        eprintln!("    [{}] fst=bool={:?} num={:?}", j, hh_b, hh_n);
                        hcur = ht;
                    }
                }

                // Describe tail briefly
                let td = describe(&arena, tail, 0);
                eprintln!("  tail = {}", if td.len() > 200 { &td[..200] } else { &td });

                // Check if tail is boolean (end of list)
                let tail_bool = decode_bool(&mut arena, tail, f.min(1_000_000));
                if let Some(b) = tail_bool {
                    eprintln!("  tail = BOOL({}) → last item", b);
                    break;
                }

                current = tail;
            }
        }
        "render1" => {
            // Render using 1-arg pair structure.
            // Theory: result is a 1-arg pair list. The first item contains
            // the image data or pixel function. Try rendering various ways.
            eprintln!("Extracting from 1-arg pair structure...");
            let mut f = remaining_fuel;

            // Get first item and rest
            let item = pair1_fst(&mut arena, result, &mut f);
            let rest = pair1_snd(&mut arena, result, &mut f);
            eprintln!("  item extracted");

            let rest_bool = decode_bool(&mut arena, rest, f.min(1_000_000));
            eprintln!("  rest is bool: {:?}", rest_bool);

            // Try item as pixel function: item(N)(m)(z)
            eprintln!("\n--- item(N)(m)(z) with N=16 ---");
            let size = render_var;
            let mut pixels = vec![128u8; (size * size) as usize];
            let mut num_count = 0u64;
            let mut bool_count = 0u64;
            let mut fail_count = 0u64;
            let fuel_per_pixel: u64 = 10_000_000;

            for m in 0..size {
                for z in 0..size {
                    let var_n = make_scott_num(&mut arena, size);
                    let m_n = make_scott_num(&mut arena, m);
                    let z_n = make_scott_num(&mut arena, z);
                    let app1 = arena.alloc(APP, item, var_n);
                    let app2 = arena.alloc(APP, app1, m_n);
                    let app3 = arena.alloc(APP, app2, z_n);
                    let mut pf = fuel_per_pixel;
                    arena.whnf(app3, &mut pf);
                    let r = arena.follow(app3);

                    if let Some(n) = decode_scott_num(&mut arena, r, 1_000_000) {
                        pixels[(m * size + z) as usize] = (n.min(255)) as u8;
                        num_count += 1;
                    } else if let Some(b) = decode_bool(&mut arena, r, 500_000) {
                        pixels[(m * size + z) as usize] = if b { 255 } else { 0 };
                        bool_count += 1;
                    } else {
                        fail_count += 1;
                    }
                }
                if (m + 1) % 4 == 0 {
                    eprint!(
                        "\r  row {}/{} ({} num, {} bool, {} fail)     ",
                        m + 1,
                        size,
                        num_count,
                        bool_count,
                        fail_count
                    );
                }
            }
            eprintln!();
            eprintln!(
                "  Decoded: {} num, {} bool, {} fail",
                num_count, bool_count, fail_count
            );

            let max_val = pixels
                .iter()
                .copied()
                .filter(|&p| p != 128)
                .max()
                .unwrap_or(1);
            let fname = format!("{}_item_{}x{}.pgm", img_path, size, size);
            write_pgm(&fname, size as usize, size as usize, &pixels);
            eprintln!("  Saved {}", fname);

            if max_val > 1 && max_val < 255 {
                let normalized: Vec<u8> = pixels
                    .iter()
                    .map(|&p| {
                        if p == 128 {
                            128
                        } else {
                            ((p as u32) * 255 / max_val as u32).min(255) as u8
                        }
                    })
                    .collect();
                let fname2 = format!("{}_item_norm_{}x{}.pgm", img_path, size, size);
                write_pgm(&fname2, size as usize, size as usize, &normalized);
                eprintln!("  Saved {}", fname2);
            }

            // Print sample
            let sample = 8.min(size);
            eprintln!("  Sample pixels:");
            for m in 0..sample {
                eprint!("    ");
                for z in 0..sample {
                    eprint!("{:3} ", pixels[(m * size + z) as usize]);
                }
                eprintln!();
            }

            // Also try: item(m)(z) without N
            eprintln!("\n--- item(m)(z) without N ---");
            for m in 0..4u64 {
                for z in 0..4u64 {
                    let m_n = make_scott_num(&mut arena, m);
                    let z_n = make_scott_num(&mut arena, z);
                    let app1 = arena.alloc(APP, item, m_n);
                    let app2 = arena.alloc(APP, app1, z_n);
                    let mut pf = f.min(10_000_000);
                    arena.whnf(app2, &mut pf);
                    let r = arena.follow(app2);
                    let num = decode_scott_num(&mut arena, r, 1_000_000);
                    let b = if num.is_none() {
                        decode_bool(&mut arena, r, 500_000)
                    } else {
                        None
                    };
                    eprint!("  ({},{})=", m, z);
                    if let Some(n) = num {
                        eprint!("N{} ", n);
                    } else if let Some(b) = b {
                        eprint!("B{} ", b as u8);
                    } else {
                        eprint!("?? ");
                    }
                }
                eprintln!();
            }

            // Also try rendering the diamond tree from item
            eprintln!("\n--- item as diamond tree ---");
            for depth in &[4usize, 8] {
                let sz = 1usize << depth;
                let mut pix = vec![255u8; sz * sz];
                let mut pc = 0u64;
                let mut rf = f.min(100_000_000);
                render_diamond(&mut arena, item, &mut pix, 0, 0, sz, sz, &mut rf, &mut pc);
                let black = pix.iter().filter(|&&p| p == 0).count();
                let white = pix.iter().filter(|&&p| p == 255).count();
                eprintln!(
                    "  {}x{}: {} rendered, black={}, white={}",
                    sz, sz, pc, black, white
                );
                let fname = format!("{}_item_diamond_{}x{}.pgm", img_path, sz, sz);
                write_pgm(&fname, sz, sz, &pix);
            }
        }
        "io" => {
            // I/O interpreter based on hint-new.md
            // Output = (tuple (tuple p1 p2) Q) = pair1(pair1(p1, p2), Q)
            // p1, p2 are Church-encoded numbers
            // p1=0: halt, p1=1: output, p1=2: input
            // Output: p2=0/1/2 for int/string/image, Q = pair1(data, continuation)
            // Input: p2=0/1 for int/string, Q = λx.continuation
            let io_start_nodes = arena.nodes.len();
            let io_dynamic_limit = io_start_nodes
                .saturating_mul(IO_ALLOC_LIMIT_GROWTH_MULTIPLIER)
                .max(io_start_nodes.saturating_add(IO_ALLOC_LIMIT_MIN_HEADROOM))
                .min(ARENA_HARD_LIMIT);
            let io_limit = io_alloc_limit
                .unwrap_or(io_dynamic_limit)
                .clamp(1, ARENA_HARD_LIMIT);
            arena.enable_io_alloc_failsafe(io_limit);
            eprintln!(
                "I/O alloc failsafe: start_nodes={}, limit_nodes={}, free_list={}",
                io_start_nodes,
                io_limit,
                arena.free_list.len()
            );

            // Quick self-test of pair2 extraction
            {
                let true_node = make_true(&mut arena);
                // let false_node = make_false(&mut arena);
                // Build a list: cons(nil, true) = pair2(false_node, true_node)
                let nil = make_false(&mut arena);
                let list1 = make_pair(&mut arena, nil, true_node);
                // pair_snd(list1) should be true_node
                let mut tf1 = 1000000u64;
                let snd1 = pair_snd(&mut arena, list1, &mut tf1);
                let snd1_bool = decode_bool(&mut arena, snd1, 1000000);
                eprintln!(
                    "SELFTEST pair2: pair_snd(cons(nil, true)) = {:?} (expected Some(true))",
                    snd1_bool
                );
                // pair_fst(list1) should be nil
                let mut tf2 = 1000000u64;
                let fst1 = pair_fst(&mut arena, list1, &mut tf2);
                let fst1_bool = decode_bool(&mut arena, fst1, 1000000);
                eprintln!(
                    "SELFTEST pair2: pair_fst(cons(nil, true)) = {:?} (expected Some(false))",
                    fst1_bool
                );
                // Build cons(list1, false) = pair2(list1, false_node)
                let false2 = make_false(&mut arena);
                let list2 = make_pair(&mut arena, list1, false2);
                let mut tf3 = 1000000u64;
                let snd2 = pair_snd(&mut arena, list2, &mut tf3);
                let snd2_bool = decode_bool(&mut arena, snd2, 1000000);
                eprintln!(
                    "SELFTEST pair2: pair_snd(cons(list1, false)) = {:?} (expected Some(false))",
                    snd2_bool
                );
                let mut tf4 = 1000000u64;
                let fst2 = pair_fst(&mut arena, list2, &mut tf4);
                // fst2 should be list1; extracting snd from it should give true
                let mut tf5 = 1000000u64;
                let fst2_snd = pair_snd(&mut arena, fst2, &mut tf5);
                let fst2_snd_bool = decode_bool(&mut arena, fst2_snd, 1000000);
                eprintln!(
                    "SELFTEST pair2: pair_snd(pair_fst(list2)) = {:?} (expected Some(true))",
                    fst2_snd_bool
                );
            }

            // SELFTEST: diamond (Church 5-tuple) selectors
            {
                let t = make_true(&mut arena); // S(KK)I
                let f = make_false(&mut arena); // KI
                                                // Build 5-tuple: (true, false, true, false, true)
                                                // Church 5-tuple: λh. h(a)(b)(c)(d)(e)
                                                // = S(S(S(S(SI)(Ka))(Kb))(Kc))(Kd))(Ke)
                let i_n = arena.alloc(I, NIL, NIL);
                // let k_n = arena.alloc(K, NIL, NIL);
                // Build S(I)(K(a)) step by step
                let ka = arena.alloc(K1, t, NIL); // K(true)
                                                  // let si = arena.alloc(S1, i_n, NIL); // S(I)
                let si_ka = arena.alloc(S2, i_n, ka); // S(I)(K(true))
                let kb = arena.alloc(K1, f, NIL); // K(false)
                let s_sika_kb = arena.alloc(S2, si_ka, kb); // S(S(I)(K(true)))(K(false))
                let kc = arena.alloc(K1, t, NIL); // K(true)
                let s2 = arena.alloc(S2, s_sika_kb, kc); // S(S(S(I)(Ka))(Kb))(Kc)
                let kd = arena.alloc(K1, f, NIL); // K(false)
                let s3 = arena.alloc(S2, s2, kd); // S(S(S(S(I)(Ka))(Kb))(Kc))(Kd)
                let ke = arena.alloc(K1, t, NIL); // K(true)
                let tuple5 = arena.alloc(S2, s3, ke); // The 5-tuple

                for i in 0..5 {
                    let sel = build_diamond_sel(&mut arena, i);
                    let app = arena.alloc(APP, tuple5, sel);
                    let mut sf = 1_000_000u64;
                    arena.whnf(app, &mut sf);
                    let r = arena.follow(app);
                    let b = decode_bool(&mut arena, r, 500000);
                    let expected = if i % 2 == 0 {
                        "Some(true)"
                    } else {
                        "Some(false)"
                    };
                    eprintln!(
                        "SELFTEST diamond sel_{}: {:?} (expected {})",
                        i, b, expected
                    );
                }
            }

            let mut current = result;
            let mut step = 0u32;
            let fuel_per_step: u64 = 50_000_000;

            loop {
                step += 1;
                if step > 100 {
                    eprintln!("Too many I/O steps, stopping.");
                    break;
                }

                // current = (tuple tag Q) = pair1(tag, Q)
                let mut f1 = fuel_per_step;
                let tag = pair1_fst(&mut arena, current, &mut f1);
                let mut f2 = fuel_per_step;
                let q = pair1_snd(&mut arena, current, &mut f2);

                // tag = (tuple p1 p2) = pair1(p1, p2)
                let mut f3 = fuel_per_step;
                let p1_node = pair1_fst(&mut arena, tag, &mut f3);
                let mut f4 = fuel_per_step;
                let p2_node = pair1_snd(&mut arena, tag, &mut f4);

                let p1 = decode_church_num(&mut arena, p1_node, fuel_per_step);
                let p2 = decode_church_num(&mut arena, p2_node, fuel_per_step);

                eprintln!("Step {}: p1={:?}, p2={:?}", step, p1, p2);
                eprintln!("  tag node: {}", describe(&arena, tag, 0));
                eprintln!("  p1 node: {}", describe(&arena, p1_node, 0));
                eprintln!("  p2 node: {}", describe(&arena, p2_node, 0));

                match p1 {
                    Some(0) => {
                        eprintln!("HALT instruction.");
                        break;
                    }
                    Some(1) => {
                        // Output instruction
                        // Q = pair1(data, continuation)
                        let mut fq1 = fuel_per_step;
                        let data = pair1_fst(&mut arena, q, &mut fq1);
                        let mut fq2 = fuel_per_step;
                        let cont = pair1_snd(&mut arena, q, &mut fq2);

                        // String list uses: cons(prev_list, value) = pair2(prev, val)
                        // pair_fst = prev_list (rest), pair_snd = value (character code)
                        // Character codes are integers encoded as: pair(bit, rest_bits)
                        // pair_fst = bit, pair_snd = rest_bits (same as decode_scott_num)
                        // Skip string scanning for image output (p2=2) to avoid OOM
                        // Try BOTH conventions for string list scan
                        for conv in if p2 == Some(2) {
                            vec![]
                        } else {
                            vec!["B_fst_val", "A_fst_rest"]
                        } {
                            let mut wd = data;
                            let mut total = 0u32;
                            let mut chars_a: Vec<Option<u64>> = Vec::new();
                            let mut chars_int: Vec<Option<i64>> = Vec::new();
                            for _i in 0..200u32 {
                                let is_nil = decode_bool(&mut arena, wd, fuel_per_step);
                                if is_nil == Some(false) {
                                    eprintln!(
                                        "  [{}] terminated at nil after {} elements",
                                        conv, total
                                    );
                                    break;
                                }
                                total += 1;
                                let (w_val, w_rest) = if conv == "B_fst_val" {
                                    // Convention B: fst=value(char), snd=rest
                                    let mut wf1 = fuel_per_step;
                                    let v = pair_fst(&mut arena, wd, &mut wf1);
                                    let mut wf2 = fuel_per_step;
                                    let r = pair_snd(&mut arena, wd, &mut wf2);
                                    (v, r)
                                } else {
                                    // Convention A: fst=rest, snd=value(char)
                                    let mut wf1 = fuel_per_step;
                                    let v = pair_snd(&mut arena, wd, &mut wf1);
                                    let mut wf2 = fuel_per_step;
                                    let r = pair_fst(&mut arena, wd, &mut wf2);
                                    (v, r)
                                };
                                let scott = decode_scott_num(&mut arena, w_val, fuel_per_step);
                                let intval = decode_integer(&mut arena, w_val, fuel_per_step);
                                if total <= 40 {
                                    let vd = describe(&arena, w_val, 0);
                                    eprintln!(
                                        "  [{}] elem[{}]: scott={:?} int={:?} val={}",
                                        conv,
                                        total - 1,
                                        scott,
                                        intval,
                                        &vd[..120.min(vd.len())]
                                    );
                                }
                                chars_a.push(scott);
                                chars_int.push(intval);
                                wd = w_rest;
                            }
                            eprintln!("  [{}] total: {} elements", conv, total);
                            // Show values
                            let vals: Vec<String> = chars_a
                                .iter()
                                .zip(chars_int.iter())
                                .map(|(s, i)| match (s, i) {
                                    (Some(n), _) => format!("{}", n),
                                    (_, Some(n)) => format!("i{}", n),
                                    _ => "?".to_string(),
                                })
                                .collect();
                            eprintln!("  [{}] values: {}", conv, vals.join(","));
                            // Try to build string from integer values
                            let mut str_chars: Vec<char> = Vec::new();
                            for i in &chars_int {
                                match i {
                                    Some(n) if *n >= 32 && *n < 127 => {
                                        str_chars.push(*n as u8 as char)
                                    }
                                    Some(n) if *n >= 0 && *n < 0x110000 => {
                                        str_chars.push(char::from_u32(*n as u32).unwrap_or('?'))
                                    }
                                    _ => str_chars.push('?'),
                                }
                            }
                            let as_is: String = str_chars.iter().collect();
                            let reversed: String = str_chars.iter().rev().collect();
                            eprintln!(
                                "  [{}] as string (outer-first): {:?}",
                                conv,
                                &as_is[..200.min(as_is.len())]
                            );
                            eprintln!(
                                "  [{}] as string (reversed): {:?}",
                                conv,
                                &reversed[..200.min(reversed.len())]
                            );
                        }

                        match p2 {
                            Some(0) => {
                                // Integer output
                                let val = decode_integer(&mut arena, data, fuel_per_step);
                                eprintln!("OUTPUT INT: {:?}", val);
                            }
                            Some(1) => {
                                // String output
                                let s = decode_string(&mut arena, data, fuel_per_step * 4);
                                eprintln!("OUTPUT STRING: {:?}", s);
                                if let Some(ref s) = s {
                                    println!("{}", s);
                                }
                            }
                            Some(2) => {
                                // Image output - Zoom renderer (hint-new-2)
                                // false = BLACK (0), true = WHITE (255)
                                // Depth 1-8: render at 2^(depth-1) x 2^(depth-1)
                                // Depth 9-25: zoom into center 1/2, render at 128x128
                                eprintln!("OUTPUT IMAGE (quadtree, zoom renderer)");
                                eprintln!("  Arena nodes: {}", arena.nodes.len());
                                // Pre-build selectors once
                                let sels: [u32; 5] = [
                                    build_diamond_sel(&mut arena, 0),
                                    build_diamond_sel(&mut arena, 1),
                                    build_diamond_sel(&mut arena, 2),
                                    build_diamond_sel(&mut arena, 3),
                                    build_diamond_sel(&mut arena, 4),
                                ];

                                use std::collections::HashMap;
                                let mut child_cache: HashMap<u32, [u32; 4]> = HashMap::new();
                                let mut bool_cache: HashMap<u32, Option<bool>> = HashMap::new();
                                let child_eval_fuel: u64 = 120_000_000;
                                let probe_bool_eval_fuel: u64 = 12_000_000;
                                let render_eval_fuel: u64 = 40_000_000;

                                // Helper: extract i-th child (1=TL, 2=TR, 3=BL, 4=BR)
                                fn get_child_fn(
                                    arena: &mut Arena,
                                    parent: u32,
                                    child_idx: usize,
                                    sels: &[u32; 5],
                                    cache: &mut HashMap<u32, [u32; 4]>,
                                    fuel: u64,
                                ) -> u32 {
                                    let p = arena.follow(parent);
                                    if let Some(children) = cache.get(&p) {
                                        return children[child_idx - 1];
                                    }
                                    let mut children = [0u32; 4];
                                    for i in 1..=4 {
                                        let app = arena.alloc(APP, p, sels[i]);
                                        let mut f = fuel;
                                        arena.whnf(app, &mut f);
                                        children[i - 1] = arena.follow(app);
                                    }
                                    cache.insert(p, children);
                                    children[child_idx - 1]
                                }

                                // Helper: get bool_b of a node
                                fn get_bool_fn(
                                    arena: &mut Arena,
                                    node: u32,
                                    sels: &[u32; 5],
                                    cache: &mut HashMap<u32, Option<bool>>,
                                    fuel: u64,
                                ) -> Option<bool> {
                                    let n = arena.follow(node);
                                    if let Some(&b) = cache.get(&n) {
                                        return b;
                                    }
                                    let app = arena.alloc(APP, n, sels[0]);
                                    let mut f = fuel;
                                    arena.whnf(app, &mut f);
                                    let cond = arena.follow(app);
                                    let b = decode_bool(arena, cond, 5_000_000);
                                    cache.insert(n, b);
                                    b
                                }

                                fn calc_gc_min_free(arena: &Arena, io_limit: usize) -> usize {
                                    let remaining = io_limit.saturating_sub(arena.nodes.len());
                                    (remaining / 3).clamp(5_000_000, 60_000_000)
                                }

                                fn gc_zoom_roots(
                                    arena: &mut Arena,
                                    sels: &[u32; 5],
                                    zoom_roots: [u32; 4],
                                    child_cache: &mut HashMap<u32, [u32; 4]>,
                                    bool_cache: &mut HashMap<u32, Option<bool>>,
                                    label: &str,
                                ) {
                                    let mut roots: Vec<u32> = Vec::new();
                                    for &s in sels {
                                        roots.push(s);
                                    }
                                    roots.extend_from_slice(&zoom_roots);
                                    for (&parent, children) in child_cache.iter() {
                                        roots.push(parent);
                                        for &c in children {
                                            roots.push(c);
                                        }
                                    }
                                    for &node in bool_cache.keys() {
                                        roots.push(node);
                                    }
                                    let (total, live, freed) = arena.gc(&roots);
                                    eprintln!(
                                        "    {} GC: total={}, live={}, freed={}, free={}",
                                        label,
                                        total,
                                        live,
                                        freed,
                                        arena.free_list.len()
                                    );
                                }

                                fn maybe_gc_zoom(
                                    arena: &mut Arena,
                                    sels: &[u32; 5],
                                    zoom_roots: [u32; 4],
                                    child_cache: &mut HashMap<u32, [u32; 4]>,
                                    bool_cache: &mut HashMap<u32, Option<bool>>,
                                    io_limit: usize,
                                    label: &str,
                                ) {
                                    let min_free = calc_gc_min_free(arena, io_limit);
                                    if arena.free_list.len() >= min_free {
                                        return;
                                    }
                                    child_cache.clear();
                                    bool_cache.clear();
                                    gc_zoom_roots(arena, sels, zoom_roots, child_cache, bool_cache, label);
                                }

                                // Render at multiple depths per the hint-new-2 strategy
                                // Depth 1-8: render at 2^(depth-1) resolution
                                // Depth 9-25: zoom into center 1/2, render at 128x128
                                let max_depth: usize = 25;

                                // We track 4 quadrant roots for the current "virtual root"
                                // For depth <= 8, we have a single root; for zoom we have 4 sub-roots
                                let root_tl = get_child_fn(
                                    &mut arena,
                                    data,
                                    1,
                                    &sels,
                                    &mut child_cache,
                                    child_eval_fuel,
                                );
                                let root_tr = get_child_fn(
                                    &mut arena,
                                    data,
                                    2,
                                    &sels,
                                    &mut child_cache,
                                    child_eval_fuel,
                                );
                                let root_bl = get_child_fn(
                                    &mut arena,
                                    data,
                                    3,
                                    &sels,
                                    &mut child_cache,
                                    child_eval_fuel,
                                );
                                let root_br = get_child_fn(
                                    &mut arena,
                                    data,
                                    4,
                                    &sels,
                                    &mut child_cache,
                                    child_eval_fuel,
                                );
                                eprintln!(
                                    "  Root children extracted. Arena: {}",
                                    arena.nodes.len()
                                );

                                // Pixel-by-pixel renderer using checkpoint/restore.
                                // For each pixel, navigate from sub_roots to the leaf, extract bool_b,
                                // then restore arena to discard all temporary nodes.
                                // This bounds memory usage to: base arena + per-pixel temporaries.
                                // Shared lazy evaluation renderer: no checkpoint/restore.
                                // Benefits from graph sharing - once a node is evaluated, it stays.
                                // Uses periodic GC to bound memory.
                                fn render_shared(
                                    arena: &mut Arena,
                                    sub_roots: [u32; 4], // [TL, TR, BL, BR]
                                    size: usize, // output image size (must be power of 2, >= 2)
                                    sels: &[u32; 5],
                                    fuel_per_pixel: u64,
                                    gc_min_free: usize,
                                ) -> Vec<u8> {
                                    let half = size / 2;
                                    let depth_within = if half <= 1 {
                                        0
                                    } else {
                                        (half as f64).log2() as usize
                                    };
                                    let mut pixels = vec![0u8; size * size];
                                    let mut white = 0usize;
                                    let mut black = 0usize;
                                    let mut gray = 0usize;

                                    for row in 0..size {
                                        for col in 0..size {
                                            let qi = if row < half {
                                                if col < half {
                                                    0
                                                } else {
                                                    1
                                                }
                                            } else {
                                                if col < half {
                                                    2
                                                } else {
                                                    3
                                                }
                                            };
                                            let mut node = sub_roots[qi];
                                            let mut local_row =
                                                if row < half { row } else { row - half };
                                            let mut local_col =
                                                if col < half { col } else { col - half };
                                            let mut local_size = half;

                                            let mut ok = true;
                                            for _level in 0..depth_within {
                                                let lh = local_size / 2;
                                                let child_idx = if local_row < lh {
                                                    if local_col < lh {
                                                        1
                                                    } else {
                                                        2
                                                    }
                                                } else {
                                                    if local_col < lh {
                                                        3
                                                    } else {
                                                        4
                                                    }
                                                };
                                                let app = arena.alloc(APP, node, sels[child_idx]);
                                                let mut f = fuel_per_pixel;
                                                arena.whnf(app, &mut f);
                                                if f == 0 {
                                                    ok = false;
                                                    break;
                                                }
                                                node = arena.follow(app);
                                                if local_row >= lh {
                                                    local_row -= lh;
                                                }
                                                if local_col >= lh {
                                                    local_col -= lh;
                                                }
                                                local_size = lh;
                                            }

                                            let pixel_val = if ok {
                                                let app = arena.alloc(APP, node, sels[0]);
                                                let mut f = fuel_per_pixel;
                                                arena.whnf(app, &mut f);
                                                if f == 0 {
                                                    128u8 // fuel exhausted
                                                } else {
                                                    let cond = arena.follow(app);
                                                    match decode_bool(arena, cond, fuel_per_pixel) {
                                                        Some(true) => 255u8,
                                                        Some(false) => 0u8,
                                                        None => 128u8,
                                                    }
                                                }
                                            } else {
                                                128u8
                                            };

                                            pixels[row * size + col] = pixel_val;
                                            match pixel_val {
                                                255 => white += 1,
                                                0 => black += 1,
                                                _ => gray += 1,
                                            }
                                        }
                                        if (row + 1) % 8 == 0 || row == size - 1 {
                                            eprintln!(
                                                "      Row {}/{}: B={} W={} G={} arena={} free={}",
                                                row + 1,
                                                size,
                                                black,
                                                white,
                                                gray,
                                                arena.nodes.len(),
                                                arena.free_list.len()
                                            );
                                        }
                                        // GC when free list runs low (every row check)
                                        if arena.free_list.len() < gc_min_free {
                                            let mut roots: Vec<u32> = Vec::new();
                                            for &s in sels {
                                                roots.push(s);
                                            }
                                            for &r in &sub_roots {
                                                roots.push(r);
                                            }
                                            let (total, live, freed) = arena.gc(&roots);
                                            eprintln!(
                                                "      GC: total={}, live={}, freed={}, free={}",
                                                total,
                                                live,
                                                freed,
                                                arena.free_list.len()
                                            );
                                        }
                                    }
                                    pixels
                                }

                                // Run GC before rendering to reclaim I/O processing garbage
                                eprintln!("  Running initial GC...");
                                gc_zoom_roots(
                                    &mut arena,
                                    &sels,
                                    [root_tl, root_tr, root_bl, root_br],
                                    &mut child_cache,
                                    &mut bool_cache,
                                    "Initial",
                                );

                                // Phase 1: SKIPPED (all depths 1-8 are all-black)
                                eprintln!("  Phase 1: SKIPPED (depths 1-8 are all-black)");

                                // Phase 2: Center zoom for depths 9-25
                                eprintln!("  Phase 2: Center zoom depths 9-25...");
                                let mut zoom_tl = root_tl;
                                let mut zoom_tr = root_tr;
                                let mut zoom_bl = root_bl;
                                let mut zoom_br = root_br;

                                // Do zoom steps 1-8 first (without rendering, just navigate to center)
                                for step in 1..=7 {
                                    maybe_gc_zoom(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        io_limit,
                                        "Pre-zoom-step",
                                    );
                                    let new_tl = get_child_fn(
                                        &mut arena,
                                        zoom_tl,
                                        4,
                                        &sels,
                                        &mut child_cache,
                                        child_eval_fuel,
                                    );
                                    maybe_gc_zoom(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        io_limit,
                                        "Mid-zoom-step",
                                    );
                                    let new_tr = get_child_fn(
                                        &mut arena,
                                        zoom_tr,
                                        3,
                                        &sels,
                                        &mut child_cache,
                                        child_eval_fuel,
                                    );
                                    let new_bl = get_child_fn(
                                        &mut arena,
                                        zoom_bl,
                                        2,
                                        &sels,
                                        &mut child_cache,
                                        child_eval_fuel,
                                    );
                                    let new_br = get_child_fn(
                                        &mut arena,
                                        zoom_br,
                                        1,
                                        &sels,
                                        &mut child_cache,
                                        child_eval_fuel,
                                    );
                                    zoom_tl = new_tl;
                                    zoom_tr = new_tr;
                                    zoom_bl = new_bl;
                                    zoom_br = new_br;
                                    eprintln!(
                                        "    Zoom step {}: arena={} free={}",
                                        step,
                                        arena.nodes.len(),
                                        arena.free_list.len()
                                    );
                                }

                                // KEY OPTIMIZATION: Clear caches and GC aggressively.
                                // Remove `data` and `root_*` from GC roots — only keep zoom subtree alive.
                                // This lets GC free the vast majority of the original tree.
                                eprintln!("  Clearing caches and running aggressive GC (zoom-only roots)...");
                                child_cache.clear();
                                bool_cache.clear();
                                gc_zoom_roots(
                                    &mut arena,
                                    &sels,
                                    [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                    &mut child_cache,
                                    &mut bool_cache,
                                    "Aggressive",
                                );

                                // Now zoom_tl/tr/bl/br represent the center of depth 8
                                // Continue zooming for depths 9-25, rendering at each depth
                                for depth in 9..=max_depth {
                                    maybe_gc_zoom(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        io_limit,
                                        "Pre-depth",
                                    );
                                    // Zoom step: extract center children
                                    let new_tl = get_child_fn(
                                        &mut arena,
                                        zoom_tl,
                                        4,
                                        &sels,
                                        &mut child_cache,
                                        child_eval_fuel,
                                    );
                                    maybe_gc_zoom(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        io_limit,
                                        "Mid-depth",
                                    );
                                    let new_tr = get_child_fn(
                                        &mut arena,
                                        zoom_tr,
                                        3,
                                        &sels,
                                        &mut child_cache,
                                        child_eval_fuel,
                                    );
                                    let new_bl = get_child_fn(
                                        &mut arena,
                                        zoom_bl,
                                        2,
                                        &sels,
                                        &mut child_cache,
                                        child_eval_fuel,
                                    );
                                    let new_br = get_child_fn(
                                        &mut arena,
                                        zoom_br,
                                        1,
                                        &sels,
                                        &mut child_cache,
                                        child_eval_fuel,
                                    );
                                    zoom_tl = new_tl;
                                    zoom_tr = new_tr;
                                    zoom_bl = new_bl;
                                    zoom_br = new_br;
                                    eprintln!(
                                        "    Zoom to depth {}: arena={} free={}",
                                        depth,
                                        arena.nodes.len(),
                                        arena.free_list.len()
                                    );

                                    // GC after zoom step (before probe/render) — clears intermediate garbage
                                    child_cache.clear();
                                    bool_cache.clear();
                                    gc_zoom_roots(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        "Post-zoom",
                                    );

                                    // Probe bool_b
                                    maybe_gc_zoom(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        io_limit,
                                        "Pre-probe",
                                    );
                                    let b_tl = get_bool_fn(
                                        &mut arena,
                                        zoom_tl,
                                        &sels,
                                        &mut bool_cache,
                                        probe_bool_eval_fuel,
                                    );
                                    maybe_gc_zoom(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        io_limit,
                                        "Mid-probe-1",
                                    );
                                    let b_tr = get_bool_fn(
                                        &mut arena,
                                        zoom_tr,
                                        &sels,
                                        &mut bool_cache,
                                        probe_bool_eval_fuel,
                                    );
                                    maybe_gc_zoom(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        io_limit,
                                        "Mid-probe-2",
                                    );
                                    let b_bl = get_bool_fn(
                                        &mut arena,
                                        zoom_bl,
                                        &sels,
                                        &mut bool_cache,
                                        probe_bool_eval_fuel,
                                    );
                                    maybe_gc_zoom(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        io_limit,
                                        "Mid-probe-3",
                                    );
                                    let b_br = get_bool_fn(
                                        &mut arena,
                                        zoom_br,
                                        &sels,
                                        &mut bool_cache,
                                        probe_bool_eval_fuel,
                                    );
                                    eprintln!("  Depth {} probe: TL={:?} TR={:?} BL={:?} BR={:?} arena={} free={}",
                                        depth, b_tl, b_tr, b_bl, b_br, arena.nodes.len(), arena.free_list.len());

                                    // GC after probe, before render
                                    gc_zoom_roots(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        "Pre-render",
                                    );

                                    // Render using shared lazy evaluation.
                                    // Keep only zoom subtree + selectors as live roots for GC.
                                    let render_sz: usize = 8;
                                    let render_gc_min_free = calc_gc_min_free(&arena, io_limit);
                                    eprintln!(
                                        "  Rendering depth {} ({}x{} center zoom, fuel={}, gc_min_free={})...",
                                        depth,
                                        render_sz,
                                        render_sz,
                                        render_eval_fuel,
                                        render_gc_min_free
                                    );
                                    let pix = render_shared(
                                        &mut arena,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        render_sz,
                                        &sels,
                                        render_eval_fuel,
                                        render_gc_min_free,
                                    );
                                    let bc = pix.iter().filter(|&&p| p == 0).count();
                                    let wc = pix.iter().filter(|&&p| p == 255).count();
                                    let gc_count = pix.iter().filter(|&&p| p == 128).count();
                                    eprintln!(
                                        "    black={}, white={}, gray={}, arena={}",
                                        bc,
                                        wc,
                                        gc_count,
                                        arena.nodes.len()
                                    );
                                    let fname = format!(
                                        "{}_depth{}_{}x{}.pgm",
                                        img_path, depth, render_sz, render_sz
                                    );
                                    write_pgm(&fname, render_sz, render_sz, &pix);
                                    eprintln!("    Saved: {}", fname);

                                    // GC after each depth — only zoom roots, NO `data`
                                    child_cache.clear();
                                    bool_cache.clear();
                                    gc_zoom_roots(
                                        &mut arena,
                                        &sels,
                                        [zoom_tl, zoom_tr, zoom_bl, zoom_br],
                                        &mut child_cache,
                                        &mut bool_cache,
                                        "Post-depth",
                                    );
                                }
                            }
                            _ => {
                                eprintln!("OUTPUT with unknown p2={:?}", p2);
                            }
                        }

                        current = cont;
                    }
                    Some(2) => {
                        // Input instruction
                        // Q = λx.continuation(x)
                        eprintln!("INPUT requested (p2={:?})", p2);
                        eprintln!(
                            "  Q node: {}",
                            &describe(&arena, q, 0)[..200.min(describe(&arena, q, 0).len())]
                        );

                        let input_val = if !key_codes.is_empty() {
                            // Build key string from --key codes
                            // B_fst_val convention: pair_fst = value (char), pair_snd = rest
                            // make_pair(a, b) -> pair_fst=a, pair_snd=b
                            // So: make_pair(char_code, rest)
                            eprintln!("  Using key codes: {:?}", key_codes);
                            let mut str_node = make_false(&mut arena); // nil
                                                                       // Push in reverse order so first char is outermost
                                                                       // (matches how the program stores strings)
                            for &code in key_codes.iter().rev() {
                                let ch_num = make_scott_num(&mut arena, code);
                                str_node = make_pair(&mut arena, ch_num, str_node);
                            }
                            eprintln!(
                                "  Built key string (B_fst_val reversed, {} chars)",
                                key_codes.len()
                            );
                            str_node
                        } else {
                            eprintln!("  No --key provided, using empty string");
                            make_false(&mut arena) // empty string = nil = KI
                        };
                        let app = arena.alloc(APP, q, input_val);
                        let mut fi = fuel_per_step;
                        arena.whnf(app, &mut fi);
                        current = arena.follow(app);
                    }
                    None => {
                        eprintln!("Failed to decode p1 as Church number.");
                        eprintln!(
                            "  Trying p1 as bool: {:?}",
                            decode_bool(&mut arena, p1_node, fuel_per_step)
                        );
                        eprintln!(
                            "  Trying p1 as Scott num: {:?}",
                            decode_scott_num(&mut arena, p1_node, fuel_per_step)
                        );

                        // Try alternative: maybe it's NOT a 1-arg tuple.
                        // Maybe the encoding uses 2-arg pairs for tuples too?
                        eprintln!("Trying 2-arg pair extraction for tag...");
                        let mut fa = fuel_per_step;
                        let p1_2arg = pair_fst(&mut arena, current, &mut fa);
                        let mut fb = fuel_per_step;
                        let p2_2arg = pair_snd(&mut arena, current, &mut fb);
                        eprintln!("  2-arg fst: {}", describe(&arena, p1_2arg, 0));
                        eprintln!(
                            "  2-arg snd: {}",
                            &describe(&arena, p2_2arg, 0)
                                [..200.min(describe(&arena, p2_2arg, 0).len())]
                        );

                        // Try Church decode on 2-arg extracted values
                        let p1_alt = decode_church_num(&mut arena, p1_2arg, fuel_per_step);
                        eprintln!("  2-arg fst as Church: {:?}", p1_alt);

                        break;
                    }
                    _ => {
                        eprintln!("Unknown p1 value: {:?}", p1);
                        break;
                    }
                }
            }
        }
        "keyfind" => {
            // Timing side-channel attack on key check.
            // Run I/O interpreter to Step 2 (input), then try each character code
            // 1-24 as a single-char input and measure reduction steps.
            // The correct character takes more steps (comparison proceeds further).

            // === Step 1: Output (question text) ===
            eprintln!("=== KEYFIND: Running I/O to reach input step ===");
            let mut current = result;
            let fuel_per_step: u64 = 50_000_000;

            // Step 1: extract and skip output
            let mut f1 = fuel_per_step;
            let tag = pair1_fst(&mut arena, current, &mut f1);
            let mut f2 = fuel_per_step;
            let q = pair1_snd(&mut arena, current, &mut f2);
            let mut f3 = fuel_per_step;
            let p1_node = pair1_fst(&mut arena, tag, &mut f3);
            let p1 = decode_church_num(&mut arena, p1_node, fuel_per_step);
            eprintln!("Step 1: p1={:?} (should be 1=output)", p1);

            // Get continuation from output
            let mut fq2 = fuel_per_step;
            let cont = pair1_snd(&mut arena, q, &mut fq2);
            current = cont;

            // Step 2: should be input
            let mut f1 = fuel_per_step;
            let tag2 = pair1_fst(&mut arena, current, &mut f1);
            let mut f2 = fuel_per_step;
            let q_input = pair1_snd(&mut arena, current, &mut f2);
            let mut f3 = fuel_per_step;
            let p1_node2 = pair1_fst(&mut arena, tag2, &mut f3);
            let p1_2 = decode_church_num(&mut arena, p1_node2, fuel_per_step);
            eprintln!("Step 2: p1={:?} (should be 2=input)", p1_2);

            if p1_2 != Some(2) {
                eprintln!("ERROR: Step 2 is not input! Aborting.");
            } else {
                eprintln!("=== Reached input step. Q node ready. ===");
                eprintln!("Arena size before tests: {} nodes", arena.nodes.len());

                // Save arena snapshot
                let saved_nodes = arena.nodes.clone();
                let q_idx = q_input; // Q = λx.continuation(x)

                // Build key one character at a time
                let mut found_key: Vec<u64> = Vec::new();
                let max_key_len = 30;

                for pos in 0..max_key_len {
                    eprintln!("\n=== Testing position {} ===", pos);
                    let mut best_char: u64 = 0;
                    let mut best_steps: u64 = 0;
                    let mut results: Vec<(u64, u64)> = Vec::new();

                    for test_char in 1u64..=24 {
                        // Restore arena
                        arena.nodes.clear();
                        arena.nodes.extend_from_slice(&saved_nodes);

                        // Build input string with found_key chars + test_char
                        let mut all_chars = found_key.clone();
                        all_chars.push(test_char);

                        // Build string as pair chain (outermost = last pushed = last char)
                        let nil = make_false(&mut arena);
                        let mut str_node = nil;
                        for &ch in all_chars.iter() {
                            let ch_num = make_scott_num(&mut arena, ch);
                            str_node = make_pair(&mut arena, ch_num, str_node);
                        }

                        // Apply Q to the string and force deep evaluation
                        let test_fuel: u64 = 500_000_000;
                        let mut total_steps: u64 = 0;

                        // Step A: Q(input) → next I/O instruction
                        let app = arena.alloc(APP, q_idx, str_node);
                        let mut remaining = test_fuel;
                        arena.whnf(app, &mut remaining);
                        total_steps += test_fuel - remaining;
                        let io_result = arena.follow(app);

                        // Step B: Extract tag = pair1_fst(result)
                        let mut fb = test_fuel;
                        let tag_r = pair1_fst(&mut arena, io_result, &mut fb);
                        total_steps += test_fuel - fb;

                        // Step C: Extract Q2 = pair1_snd(result)
                        let mut fc = test_fuel;
                        let q2 = pair1_snd(&mut arena, io_result, &mut fc);
                        total_steps += test_fuel - fc;

                        // Step D: Extract p1 from tag
                        let mut fd = test_fuel;
                        let p1_r = pair1_fst(&mut arena, tag_r, &mut fd);
                        total_steps += test_fuel - fd;

                        // Step E: Decode p1 as Church number
                        let fe = test_fuel;
                        let p1_val = decode_church_num(&mut arena, p1_r, fe);
                        // Note: decode_church_num uses its own fuel internally

                        // Step F: Extract data from Q2 = pair1_fst(Q2)
                        let mut ff = test_fuel;
                        let data_r = pair1_fst(&mut arena, q2, &mut ff);
                        total_steps += test_fuel - ff;

                        // Step G: Force-evaluate the data by reading first element
                        // This triggers the lazy comparison
                        let is_nil = decode_bool(&mut arena, data_r, test_fuel / 10);
                        let mut fg = test_fuel;
                        if is_nil != Some(false) {
                            // Not nil → extract first char to force comparison
                            let first_elem = pair_fst(&mut arena, data_r, &mut fg);
                            total_steps += test_fuel - fg;
                            // Decode the first char as Scott number
                            let char_val = decode_scott_num(&mut arena, first_elem, test_fuel / 10);
                            if pos == 0 && (test_char <= 3 || test_char == 5) {
                                eprintln!(
                                    "  char={}: p1={:?}, first_output_char={:?}, total_steps={}",
                                    test_char, p1_val, char_val, total_steps
                                );
                            }
                        }

                        results.push((test_char, total_steps));

                        if total_steps > best_steps {
                            best_steps = total_steps;
                            best_char = test_char;
                        }
                    }

                    // Sort by steps and report
                    results.sort_by(|a, b| b.1.cmp(&a.1));
                    eprintln!("Position {} results (top 5):", pos);
                    for (i, (ch, steps)) in results.iter().take(5).enumerate() {
                        eprintln!("  #{}: char={} steps={}", i + 1, ch, steps);
                    }

                    // Check if there's a clear winner (significantly more steps than second)
                    if results.len() >= 2 {
                        let top = results[0].1;
                        let second = results[1].1;
                        let ratio = if second > 0 {
                            top as f64 / second as f64
                        } else {
                            999.0
                        };
                        eprintln!("  Top/second ratio: {:.3}", ratio);

                        if ratio < 1.01 {
                            // All characters take similar steps → key might be complete
                            eprintln!(
                                "  No clear winner → key might be complete at length {}",
                                pos
                            );

                            // Try the current key (without the test char) as the full key
                            // Check if it produces a non-error response
                            arena.nodes.clear();
                            arena.nodes.extend_from_slice(&saved_nodes);

                            let nil = make_false(&mut arena);
                            let mut str_node = nil;
                            for &ch in found_key.iter() {
                                let ch_num = make_scott_num(&mut arena, ch);
                                str_node = make_pair(&mut arena, ch_num, str_node);
                            }

                            let test_fuel: u64 = 500_000_000;
                            let app = arena.alloc(APP, q_idx, str_node);
                            let mut remaining = test_fuel;
                            arena.whnf(app, &mut remaining);
                            let current_result = arena.follow(app);

                            // Check if the result is the error message or something different
                            let mut fq1 = fuel_per_step;
                            let tag_r = pair1_fst(&mut arena, current_result, &mut fq1);
                            let mut fq2 = fuel_per_step;
                            let q_r = pair1_snd(&mut arena, current_result, &mut fq2);
                            let mut fq3 = fuel_per_step;
                            let p1_r = pair1_fst(&mut arena, tag_r, &mut fq3);
                            let p1_val = decode_church_num(&mut arena, p1_r, fuel_per_step);
                            eprintln!("  Full key test: p1={:?}", p1_val);

                            if p1_val == Some(1) {
                                // Output instruction → might be outputting key data!
                                let mut fd = fuel_per_step;
                                let data_r = pair1_fst(&mut arena, q_r, &mut fd);
                                let mut fp = fuel_per_step;
                                let p2_r = pair1_snd(&mut arena, tag_r, &mut fp);
                                let p2_val = decode_church_num(&mut arena, p2_r, fuel_per_step);
                                eprintln!("  Output type p2={:?}", p2_val);

                                if p2_val == Some(1) {
                                    // String output
                                    let s = decode_string(&mut arena, data_r, fuel_per_step * 4);
                                    eprintln!("  Output string: {:?}", s);
                                }
                            }

                            break;
                        }
                    }

                    found_key.push(best_char);
                    eprintln!(
                        "  Best char for position {}: {} (steps: {})",
                        pos, best_char, best_steps
                    );
                    eprintln!("  Key so far: {:?}", found_key);
                }

                eprintln!("\n=== KEYFIND RESULT ===");
                eprintln!("Found key codes: {:?}", found_key);
            }
        }
        _ => {
            eprintln!("Unknown decode mode: {}", decode_mode);
        }
    }
}

/// Build Scott-encoded number in arena.
fn make_scott_num(arena: &mut Arena, n: u64) -> u32 {
    // 0 = pair(false, nil), NOT bare nil
    let nil = make_false(arena);
    let false_node = make_false(arena);
    let zero = make_pair(arena, false_node, nil);

    if n == 0 {
        return zero;
    }

    let mut bits = Vec::new();
    let mut temp = n;
    while temp > 0 {
        bits.push(temp & 1);
        temp >>= 1;
    }
    // Build from MSB to LSB (reversed bits, build pair chain)
    // Terminate with pair(false, nil) = zero, NOT bare nil
    let mut result = zero;
    for &bit in bits.iter().rev() {
        let bit_node = if bit == 1 {
            make_true(arena)
        } else {
            make_false(arena)
        };
        result = make_pair(arena, bit_node, result);
    }
    result
}

/// Recursively decode a pair structure.
fn deep_decode(arena: &mut Arena, node: u32, fuel: u64, depth: usize, max_depth: usize) {
    if depth > max_depth || fuel == 0 {
        return;
    }

    let indent = "  ".repeat(depth);

    // Check if boolean
    match decode_bool(arena, node, fuel / 10) {
        Some(true) => {
            println!("{}TRUE", indent);
            return;
        }
        Some(false) => {
            println!("{}FALSE (nil)", indent);
            return;
        }
        None => {}
    }

    // Check if number
    if let Some(n) = decode_scott_num(arena, node, fuel / 10) {
        if n > 0 {
            println!("{}NUMBER({})", indent, n);
            return;
        }
    }

    // Try as pair (2-arg Scott pair extraction)
    let mut f1 = fuel / 4;
    let fst = pair_fst(arena, node, &mut f1);

    let mut f2 = fuel / 4;
    let snd = pair_snd(arena, node, &mut f2);

    println!("{}PAIR(", indent);
    deep_decode(arena, fst, fuel / 4, depth + 1, max_depth);
    println!("{},", indent);
    deep_decode(arena, snd, fuel / 4, depth + 1, max_depth);
    println!("{})", indent);
}

/// Collect boolean leaves from a pair tree by DFS.
/// Treats pairs as internal nodes, booleans as leaves.
fn collect_bool_leaves(
    arena: &mut Arena,
    node: u32,
    fuel: &mut u64,
    leaves: &mut Vec<u8>,
    max_leaves: usize,
) {
    if leaves.len() >= max_leaves || *fuel == 0 {
        return;
    }

    // Check if boolean leaf
    let b = decode_bool(arena, node, (*fuel).min(100000));
    match b {
        Some(true) => {
            leaves.push(1);
            return;
        }
        Some(false) => {
            leaves.push(0);
            return;
        }
        None => {}
    }

    // Not a boolean - treat as pair and recurse
    let fst = pair_fst(arena, node, fuel);
    collect_bool_leaves(arena, fst, fuel, leaves, max_leaves);

    if leaves.len() >= max_leaves {
        return;
    }

    let snd = pair_snd(arena, node, fuel);
    collect_bool_leaves(arena, snd, fuel, leaves, max_leaves);
}

/// Write PGM image file.
fn write_pgm(filename: &str, width: usize, height: usize, pixels: &[u8]) {
    let mut f = fs::File::create(filename).expect("failed to create PGM file");
    let header = format!("P5\n{} {}\n255\n", width, height);
    f.write_all(header.as_bytes()).expect("write header");
    f.write_all(pixels).expect("write pixels");
}

/// Extract pair's first element: pair(K)(dummy) → A
/// 2-arg Scott pair needs TWO arguments to extract.
fn pair_fst(arena: &mut Arena, node: u32, fuel: &mut u64) -> u32 {
    let k_sel = arena.intern_k();
    let app1 = arena.alloc(APP, node, k_sel);
    let dummy = arena.intern_i(); // dummy second arg (ignored by pair)
    let app2 = arena.alloc(APP, app1, dummy);
    arena.whnf(app2, fuel);
    arena.follow(app2)
}

/// Extract pair's second element: pair(KI)(dummy) → B
/// 2-arg Scott pair needs TWO arguments to extract.
fn pair_snd(arena: &mut Arena, node: u32, fuel: &mut u64) -> u32 {
    let ki = arena.intern_ki();
    let app1 = arena.alloc(APP, node, ki);
    let dummy = arena.intern_i(); // dummy second arg (ignored by pair)
    let app2 = arena.alloc(APP, app1, dummy);
    arena.whnf(app2, fuel);
    arena.follow(app2)
}

/// 1-arg pair extraction: node(K) → first element
/// For 1-arg Scott pairs: S(SI(KA))(KB)(K) = K(A)(B) = A
fn pair1_fst(arena: &mut Arena, node: u32, fuel: &mut u64) -> u32 {
    let k_sel = arena.intern_k();
    let app = arena.alloc(APP, node, k_sel);
    arena.whnf(app, fuel);
    arena.follow(app)
}

/// 1-arg pair extraction: node(KI) → second element
/// For 1-arg Scott pairs: S(SI(KA))(KB)(KI) = KI(A)(B) = B
fn pair1_snd(arena: &mut Arena, node: u32, fuel: &mut u64) -> u32 {
    let ki = arena.intern_ki();
    let app = arena.alloc(APP, node, ki);
    arena.whnf(app, fuel);
    arena.follow(app)
}

/// Decode a Church numeral: Church n applied to f and x gives f^n(x).
/// We apply to unique markers and count the chain depth.
fn decode_church_num(arena: &mut Arena, node: u32, fuel: u64) -> Option<u64> {
    let f_marker = arena.alloc(110, NIL, NIL);
    let x_marker = arena.alloc(111, NIL, NIL);
    let app1 = arena.alloc(APP, node, f_marker);
    let app2 = arena.alloc(APP, app1, x_marker);
    let mut f = fuel;
    arena.whnf(app2, &mut f);
    let mut cur = arena.follow(app2);
    let mut count = 0u64;
    loop {
        if f == 0 {
            return None;
        }
        let tag = arena.nodes[cur as usize].tag;
        if tag == 111 {
            return Some(count);
        }
        if tag == IND {
            cur = arena.follow(cur);
            continue;
        }
        if tag == APP {
            let func = arena.follow(arena.nodes[cur as usize].a);
            if arena.nodes[func as usize].tag == 110 {
                count += 1;
                // The argument (b) might be unreduced, e.g. APP(I, x_marker)
                // Force its evaluation
                let arg = arena.nodes[cur as usize].b;
                arena.whnf(arg, &mut f);
                cur = arena.follow(arg);
                continue;
            }
            // func is not f_marker — try reducing this node further
            arena.whnf(cur, &mut f);
            cur = arena.follow(cur);
            continue;
        }
        // Other tags (S, K, I, S1, S2, K1) — try to reduce
        // This shouldn't happen if the Church numeral is well-formed
        return None;
    }
}

/// Decode an integer from a list of booleans (two's complement).
/// Bit list convention: pair(bit, rest_bits) where pair_fst=bit, pair_snd=rest.
/// (Same convention as decode_scott_num: fst=value, snd=rest.)
/// nil (KI=false) terminates the list.
/// Bits are collected from outermost (first extracted) to innermost.
/// If outermost bit is LSB (built by pushing LSB first), we get [LSB, ..., MSB/sign].
fn decode_integer(arena: &mut Arena, node: u32, fuel: u64) -> Option<i64> {
    let mut bits: Vec<bool> = Vec::new();
    let mut current = node;
    let fuel_per_op = (fuel / 200).max(10000);
    let mut remaining = fuel;

    for _iter in 0..64 {
        if remaining < fuel_per_op * 4 {
            break;
        }

        // Check if current is nil (false/KI)
        let is_nil = decode_bool(arena, current, fuel_per_op);
        if is_nil == Some(false) {
            break; // empty list = nil, end of bits
        }

        // Extract bit (fst) and rest (snd) — same convention as Scott numbers
        let mut f1 = fuel_per_op;
        let bit_node = pair_fst(arena, current, &mut f1);
        remaining = remaining.saturating_sub(fuel_per_op - f1);

        let bit = decode_bool(arena, bit_node, fuel_per_op);
        match bit {
            Some(b) => bits.push(b),
            None => return None,
        }

        let mut f2 = fuel_per_op;
        let rest = pair_snd(arena, current, &mut f2);
        remaining = remaining.saturating_sub(fuel_per_op - f2);
        current = rest;
    }

    if bits.is_empty() {
        return Some(0);
    }

    // bits[0] is from outermost (first extracted bit), likely LSB.
    // Last extracted bit (before nil) is the sign bit in two's complement.
    // Actually, in the Scott number encoding: pair(bit0, pair(bit1, ... pair(false, nil)...))
    // decode_scott_num treats this as bit0=LSB, and pair(false, nil) as terminator.
    // For two's complement, the last bit before nil is the sign bit.
    // So bits = [bit0(LSB), bit1, ..., bitN(MSB/sign)]
    // But the last bit is the TERMINATOR false in decode_scott_num.
    // In two's complement: the list just has the bits without a separate terminator.
    // The last bit IS the sign bit.

    // Interpretation: bits = [LSB, ..., MSB/sign]
    // let sign = bits[bits.len() - 1];
    let mut n: i64 = 0;
    for (i, &b) in bits.iter().enumerate() {
        if i == bits.len() - 1 {
            // Sign bit
            if b {
                n -= 1i64 << i;
            }
        } else if b {
            n |= 1i64 << i;
        }
    }
    Some(n)
}

/// Decode a string from a list of integers (character codes).
/// Convention B: pair_fst=value(char), pair_snd=rest.
fn decode_string(arena: &mut Arena, node: u32, fuel: u64) -> Option<String> {
    let mut chars: Vec<char> = Vec::new();
    let mut current = node;
    let fuel_per_op = (fuel / 100).max(100000);
    let mut remaining = fuel;

    for _ in 0..10000 {
        if remaining < fuel_per_op * 6 {
            break;
        }

        // Check if nil
        let is_nil = decode_bool(arena, current, fuel_per_op);
        if is_nil == Some(false) {
            break; // empty list
        }

        // Convention B: fst=value(char), snd=rest
        let mut f1 = fuel_per_op;
        let char_val = pair_fst(arena, current, &mut f1);
        remaining = remaining.saturating_sub(fuel_per_op - f1);

        let mut f2 = fuel_per_op;
        let prev = pair_snd(arena, current, &mut f2);
        remaining = remaining.saturating_sub(fuel_per_op - f2);

        // Decode char as integer — try Scott number first (the program's native encoding)
        if chars.len() < 5 {
            let desc = describe(arena, char_val, 0);
            eprintln!(
                "  char[{}] val node: {}",
                chars.len(),
                &desc[..200.min(desc.len())]
            );
        }
        // Try Scott number decoding (pair-chain binary encoding from the program)
        let ch_scott = decode_scott_num(arena, char_val, fuel_per_op * 3);
        if chars.len() < 5 {
            eprintln!("  char[{}] as Scott num: {:?}", chars.len(), ch_scott);
        }
        let ch = if let Some(n) = ch_scott {
            Some(n as i64)
        } else {
            decode_integer(arena, char_val, fuel_per_op * 3)
        };
        if chars.len() < 5 {
            eprintln!("  char[{}] decoded: {:?}", chars.len(), ch);
        }
        match ch {
            Some(code) if code >= 0 && code < 0x110000 => {
                if let Some(c) = char::from_u32(code as u32) {
                    chars.push(c);
                } else {
                    chars.push('?');
                }
            }
            Some(code) => {
                eprintln!("  char code out of range: {}", code);
                chars.push('?');
            }
            None => {
                eprintln!("  failed to decode char");
                chars.push('?');
            }
        }

        current = prev;
    }

    // chars is collected outermost-first.
    // If the string is built by pushing chars one at a time,
    // outermost = last-pushed char.
    // For a string "abc": push 'a', push 'b', push 'c'.
    // outermost gives 'c', then 'b', then 'a'. So we need to reverse.
    chars.reverse();
    Some(chars.into_iter().collect())
}

/// Render diamond quadtree to pixel buffer.
/// Diamond encoding (5-element):
///   PAIR(cond, PAIR(qa, PAIR(qb, PAIR(qc, qd))))
///   cond = boolean pixel value at this level
///   qa = m-1, z-1 (top-left/NW)
///   qb = m-1, z+1 (top-right/NE)
///   qc = m+1, z-1 (bottom-left/SW)
///   qd = m+1, z+1 (bottom-right/SE)
/// Leaf: FALSE (white) or TRUE (black)
fn render_diamond(
    arena: &mut Arena,
    node: u32,
    pixels: &mut [u8],
    x: usize,
    y: usize,
    size: usize,
    img_width: usize,
    fuel: &mut u64,
    count: &mut u64,
) {
    if *fuel == 0 || size == 0 {
        return;
    }

    // Check if it's a boolean leaf
    let is_bool = decode_bool(arena, node, (*fuel).min(200000));
    match is_bool {
        Some(false) => {
            fill_rect(pixels, x, y, size, 255, img_width); // white
            *count += (size * size) as u64;
            return;
        }
        Some(true) => {
            fill_rect(pixels, x, y, size, 0, img_width); // black
            *count += (size * size) as u64;
            return;
        }
        None => {}
    }

    // At pixel level, extract condition for color
    if size <= 1 {
        let cond = pair_fst(arena, node, fuel);
        let b = decode_bool(arena, cond, (*fuel).min(200000));
        let color = match b {
            Some(true) => 0u8,    // black
            Some(false) => 255u8, // white
            None => 128u8,        // gray (unknown)
        };
        if x < img_width && y < img_width {
            pixels[y * img_width + x] = color;
        }
        *count += 1;
        return;
    }

    // Diamond structure: PAIR(cond, PAIR(qa, PAIR(qb, PAIR(qc, qd))))
    let rest = pair_snd(arena, node, fuel); // PAIR(qa, PAIR(qb, PAIR(qc, qd)))
    let qa = pair_fst(arena, rest, fuel); // qa (NW: m-1, z-1)
    let rest2 = pair_snd(arena, rest, fuel); // PAIR(qb, PAIR(qc, qd))
    let qb = pair_fst(arena, rest2, fuel); // qb (NE: m-1, z+1)
    let rest3 = pair_snd(arena, rest2, fuel); // PAIR(qc, qd)
    let qc = pair_fst(arena, rest3, fuel); // qc (SW: m+1, z-1)
    let qd = pair_snd(arena, rest3, fuel); // qd (SE: m+1, z+1)

    let half = size / 2;
    render_diamond(arena, qa, pixels, x, y, half, img_width, fuel, count);
    render_diamond(arena, qb, pixels, x + half, y, half, img_width, fuel, count);
    render_diamond(arena, qc, pixels, x, y + half, half, img_width, fuel, count);
    render_diamond(
        arena,
        qd,
        pixels,
        x + half,
        y + half,
        half,
        img_width,
        fuel,
        count,
    );
}

/// Alternate interpretation: PAIR(PAIR(nw, ne), PAIR(sw, se))
fn render_quadtree_v2(
    arena: &mut Arena,
    node: u32,
    pixels: &mut [u8],
    x: usize,
    y: usize,
    size: usize,
    img_width: usize,
    fuel: &mut u64,
    count: &mut u64,
) {
    if *fuel == 0 || size == 0 {
        return;
    }

    let is_bool = decode_bool(arena, node, (*fuel).min(200000));
    match is_bool {
        Some(false) => {
            fill_rect(pixels, x, y, size, 255, img_width);
            *count += (size * size) as u64;
            return;
        }
        Some(true) => {
            fill_rect(pixels, x, y, size, 0, img_width);
            *count += (size * size) as u64;
            return;
        }
        None => {}
    }

    if size <= 1 {
        if x < img_width && y < img_width {
            pixels[y * img_width + x] = 128;
        }
        *count += 1;
        return;
    }

    let top = pair_fst(arena, node, fuel);
    let bottom = pair_snd(arena, node, fuel);
    let nw = pair_fst(arena, top, fuel);
    let ne = pair_snd(arena, top, fuel);
    let sw = pair_fst(arena, bottom, fuel);
    let se = pair_snd(arena, bottom, fuel);

    let half = size / 2;
    render_quadtree_v2(arena, nw, pixels, x, y, half, img_width, fuel, count);
    render_quadtree_v2(arena, ne, pixels, x + half, y, half, img_width, fuel, count);
    render_quadtree_v2(arena, sw, pixels, x, y + half, half, img_width, fuel, count);
    render_quadtree_v2(
        arena,
        se,
        pixels,
        x + half,
        y + half,
        half,
        img_width,
        fuel,
        count,
    );
}

/// Build a 5-tuple selector in the arena: sel_i(a)(b)(c)(d)(e) = <i-th arg>
/// Diamond structure: diamond(COND)(QA)(QB)(QC)(QD) = λf. f(COND)(QA)(QB)(QC)(QD)
/// So data(sel_i) extracts the i-th field.
///
/// Selectors derived:
///   sel_0 = S(KK)(S(KK)(S(KK)(S(KK)(I))))
///   sel_1 = K(S(KK)(S(KK)(S(KK)(I))))
///   sel_2 = K(K(S(KK)(S(KK)(I))))
///   sel_3 = K(K(K(S(KK)(I))))
///   sel_4 = K(K(K(K(I))))
fn build_diamond_sel(arena: &mut Arena, pos: usize) -> u32 {
    arena.intern_diamond_sel(pos)
}

/// Fill a rectangular region with a color.
fn fill_rect(pixels: &mut [u8], x: usize, y: usize, size: usize, color: u8, img_width: usize) {
    for dy in 0..size {
        for dx in 0..size {
            let px = x + dx;
            let py = y + dy;
            if px < img_width && py < img_width {
                pixels[py * img_width + px] = color;
            }
        }
    }
}
