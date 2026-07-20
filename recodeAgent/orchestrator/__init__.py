"""recodeAgent orchestrator: the small deterministic Apache Burr layer that
sequences the GitHub Copilot CLI agents through the ReCodeAgent workflow.

Burr owns sequencing, the milestone x repair loop, typed state, crash-resume and
the telemetry UI. It never calls an LLM -- Copilot CLI is the agent runtime.
"""
