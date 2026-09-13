---
# `title{c}` names c, a descendant that reaches the divergent `title` at BOTH
# origins (a: String, b: Number). Which shape it checks against is arbitrary,
# so the qualifier is rejected as ambiguous — name a declaring origin directly
# (title{a} / title{b}). Filling no origin, both required origins also fire.
type: c
title{c}: "x"
---
