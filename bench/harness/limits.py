"""How many model calls one Hermes trial can have open at once, and the key
proxy's connection caps sized from it.

The bench runs Hermes v0.23.1 one-shot (`hermes chat -q`). A one-shot run
joins its subagents inside the delegate_task call and may spawn
delegation.oneshot_max_children of them in all (Hermes's default, 2, pinned
here): at most 2 at once beside the parent's own call and Hermes's
auxiliary calls (the session title, context compression), so 5.

The caps are sized for more than that, so they hold even if the bench ever
ran Hermes interactively: there every top-level delegate_task runs in the
background, a trial may hold delegation.max_concurrent_children background
units at once, each a batch of up to that many children (there is no
separate background limit; delegation.max_async_children is ignored).
Hermes's default is 10, so up to 100 children at once; the bench pins 4,
so at most the parent, 2 auxiliary calls, 4 batches of 4 and one batch run
in the turn: 23. Each call is one connection to the proxy, which answers
HTTP/1.0 and closes.
"""

ONESHOT_CHILDREN = 2
DELEGATION_CHILDREN = 4
AUXILIARY_CALLS = 2
WORST_CASE_CALLS = 1 + AUXILIARY_CALLS + DELEGATION_CHILDREN * DELEGATION_CHILDREN + DELEGATION_CHILDREN

# A trial reaches the proxy from its own address; twice its worst case
# leaves room for retries racing a closing connection.
MAX_PER_CLIENT = 2 * WORST_CASE_CALLS + 2
# Enough for every trial a bench machine runs at once (`just bench`'s n,
# up to 10) at its full share.
MAX_CONNECTIONS = 10 * MAX_PER_CLIENT
