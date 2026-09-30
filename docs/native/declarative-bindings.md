# React-free native binding experiment

The `dsc-native` target accepts one exported component with numeric local `let` state, a JSX return and inline assignment handlers. No React import, hooks or reconciler executes this component.

```tsx
export fn Counter() {
    let count = 0;
    return (
        <view className="p-6 gap-4">
            <p>Count: {count}</p>
            <button onClick={fn() { count += 1; }}>Add one</button>
        </view>
    );
}
```

Run `dsc-native program counter.dsx` to emit the checked native program. Use the native development host or browser native tour to execute it. This uses the existing DekaScript parser/checker; it is an isolated target lowering, not a change to general JavaScript emission or module semantics. The return type is inferred: `View` is not a new built-in language type.

Within this target, top-level numeric `let` declarations become instance-local state slots. An event may make one assignment using `=`, `+=`, `-=` or `*=`. Expressions support numeric state, literals, `+`, `-` and `*`. A numeric child such as `{count}` or `{count * 2}` becomes a text binding. Unsupported operations fail compilation. Legacy numeric `useState` examples remain accepted for compatibility.

`view` currently establishes the root surface, filling the supplied viewport by default, with the same column/start/stretch layout as containers. Explicit classes may override its size and layout. Nested views, focus scopes and navigation are not implemented. `div` arranges children; `p` and `span` use inline text flow. Components from other modules, nested component instances, props, general reactive expressions, lists and async effects remain future work.

The generated program is a declarative template plus state expressions and event updates. The Rust native program host retains its nodes and indexes text/style dependencies. State changes evaluate affected bindings. This does not yet guarantee incremental layout or paint: the renderer still takes a tree snapshot and lays out the scene. Legacy conditional presence may rebuild the retained tree; structural reconciliation is not part of this counter proof. The Rust source emitter also retains its existing per-render construction; the binding demo specifically exercises the native program host in desktop development mode and WASM.

The counter fixture and compiler tests live in `crates/deka_native_compile/examples/bindings.dsx` and `tests/native.rs`. No general language syntax was added, so the language/parser corpus is unchanged.
