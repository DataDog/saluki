import sys

from datadog_checks.checks import AgentCheck

class ChurnCheck(AgentCheck):
    def check(self, instance):
        self.gauge('computed_value', 42, tags=['hello:world'])
