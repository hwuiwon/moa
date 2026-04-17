# Chapter 07 Notes: Type State Pattern

## When It Helps

- Use type state when certain operations should only compile in specific states.
- This is strongest in builder APIs, connection lifecycles, and stateful resource wrappers.

## When To Avoid It

- Do not force type state into trivial enums or small state machines that are clearer as normal runtime checks.
- Avoid it when the generics become more complex than the safety gain justifies.

## MOA Translation

- This pattern may be useful in hands, session lifecycle helpers, approval states, or builder-like setup flows where invalid transitions are expensive.
- Do not retrofit type state into every workflow enum. Use it only where compile-time illegal states materially improve correctness.

## Review Questions

- Is the current API exposing invalid operations that could be made unrepresentable?
- Is the added generic complexity still readable to the next maintainer?
- Would explicit runtime validation be simpler and just as safe here?
