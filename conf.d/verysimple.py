import AgentCheck

class ChurnCheck(AgentCheck):
    def check(self, instance):
        print("Hello world")
