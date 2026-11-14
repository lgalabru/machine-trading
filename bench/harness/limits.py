"""How many model calls one Hermes trial can have open at once, and the key
proxy's connection caps sized from it.

Hermes v0.23.1 runs every top-level delegate_task in the background and
lets a trial hold delegation.max_concurrent_children background units at
once, each a batch of up to that many children running in parallel (there
is no separate background limit: delegation.max_async_children is ignored).
Hermes's default is 10, so up to 100 children at once; the bench pins 4
for every model, so delegation is bounded and the same everywhere.

At most, then: the parent's own call, Hermes's auxiliary calls beside it
(the session title, context compression), every background unit's
children, and one more batch run in the turn (a subagent orchestrating,
or a dispatch Hermes runs inline). Each call is one connection to the
proxy, which answers HTTP/1.0 and closes.
"""

DELEGATION_CHILDREN = 4
AUXILIARY_CALLS = 2
WORST_CASE_CALLS = 1 + AUXILIARY_CALLS + DELEGATION_CHILDREN * DELEGATION_CHILDREN + DELEGATION_CHILDREN

# A trial reaches the proxy from its own address; twice its worst case
# leaves room for retries racing a closing connection.
MAX_PER_CLIENT = 2 * WORST_CASE_CALLS + 2
# Enough for every trial a bench machine runs at once (`just bench`'s n,
# up to 10) at its full share.
MAX_CONNECTIONS = 10 * MAX_PER_CLIENT
