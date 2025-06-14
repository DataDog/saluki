from datadog_checks.checks import AgentCheck


class SimpleCheck(AgentCheck):
    def __init__(self, name, init_config, instances):
        super(SimpleCheck, self).__init__(name, init_config, instances)
        print("Init config: {}".format(init_config))

    def check(self, instance):
        self.gauge(
            "computed_value",
            42,
            tags=["hello:world", "argument:{}".format(instance.get("argument"))],
        )
