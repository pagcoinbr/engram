#!/usr/bin/env python3
"""A long memory at reasoning_effort=high can burn the whole max_tokens budget on
hidden CoT and come back finish_reason=length with empty content. Graphiti then
retries the identical call 4x (~12 min each on a CPU-offloaded local model) and the
memory is silently absent from the graph. _inject_reasoning must degrade the effort
and re-issue instead. Observed live: prompt 8387 tok, stop at 24770 = exactly the
16384-token cap, 4 dead retries, cypher-bringup.md dropped."""
import asyncio, sys, types
from pathlib import Path

MG = Path(__file__).resolve().parent.parent / "graph" / "mg_config.py"


def _inject_reasoning(effort):
    txt = MG.read_text()
    src = "def _inject_reasoning" + txt.split("def _inject_reasoning", 1)[1].split("def _llm_timeout")[0]
    ns = {"REASONING_EFFORT": effort}
    exec(compile(src, str(MG), "exec"), ns)
    return ns["_inject_reasoning"]


def _llm(*finish_reasons):
    """Fake client whose Nth call returns the Nth (finish_reason, content) pair."""
    calls = []
    replies = list(finish_reasons)

    async def create(**kw):
        calls.append(kw.get("extra_body", {}).get("reasoning_effort"))
        fr, content = replies[min(len(calls), len(replies)) - 1]
        msg = types.SimpleNamespace(content=content)
        return types.SimpleNamespace(choices=[types.SimpleNamespace(finish_reason=fr, message=msg)])

    llm = types.SimpleNamespace(client=types.SimpleNamespace(
        chat=types.SimpleNamespace(completions=types.SimpleNamespace(create=create))))
    return llm, calls


def main():
    # 1. happy path: one call, at the configured effort, no degrade
    llm, calls = _llm(("stop", '{"entities": []}'))
    _inject_reasoning("high")(llm)
    r = asyncio.run(llm.client.chat.completions.create(messages=[]))
    assert calls == ["high"], calls
    assert r.choices[0].message.content == '{"entities": []}'

    # 2. CoT starvation: length + empty -> retried once with thinking OFF ("low" is a
    #    no-op on this model: high and low return byte-identical CoT)
    llm, calls = _llm(("length", ""), ("stop", '{"entities": [1]}'))
    _inject_reasoning("high")(llm)
    r = asyncio.run(llm.client.chat.completions.create(messages=[]))
    assert calls == ["high", "none"], calls
    assert r.choices[0].message.content == '{"entities": [1]}'

    # 3. length but content present (truncated JSON) is NOT our case — let graphiti's
    #    own retry own it rather than burning a second 12-minute call.
    llm, calls = _llm(("length", '{"entit'))
    _inject_reasoning("high")(llm)
    asyncio.run(llm.client.chat.completions.create(messages=[]))
    assert calls == ["high"], calls

    print("ok — reasoning degrade on empty length-capped response")


if __name__ == "__main__":
    sys.exit(main())
