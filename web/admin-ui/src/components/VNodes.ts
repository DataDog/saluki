const VNodes = (props: any) => {
  return props.node;
};

VNodes.props = {
  node: {
    required: true,
  },
};

export default VNodes;
