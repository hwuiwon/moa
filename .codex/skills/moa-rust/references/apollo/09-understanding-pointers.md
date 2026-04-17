# Chapter 09 Notes: Understanding Pointers

## Core Reminders

- `Arc` is for shared ownership across threads.
- `Rc` and `RefCell` are single-threaded tools.
- `Mutex` and `RwLock` are for shared mutable state across threads.
- `Send` and `Sync` constraints matter across async task and thread boundaries.
- Interior mutability is a tool, not a default.

## MOA Translation

- In async runtime code, prefer `Arc<T>` for shared read-mostly state and `Arc<Mutex<T>>` or `Arc<RwLock<T>>` only when mutation is actually shared.
- Avoid `Rc` and `RefCell` in Tokio or cross-thread paths.
- For provider, orchestrator, and registry state, verify `Send + Sync` requirements before introducing boxed error types or pointer indirection.
- Raw pointers are essentially never the right answer in normal MOA code.

## Review Questions

- Does this shared state really need mutation, or would immutable sharing work?
- Is `Arc<Mutex<T>>` being used out of habit rather than necessity?
- Are `Send + Sync` requirements satisfied for tasks, streams, and error types?
- Is there a simpler ownership model that avoids interior mutability?
