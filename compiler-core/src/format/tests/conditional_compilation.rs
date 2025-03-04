use crate::{assert_format, assert_format_rewrite};

#[test]
fn if_rewrite() {
    assert_format_rewrite!(
        "type X

if erlang {
  type Y {
    Y
  }

  type Z {
    Z
  }
}

if javascript {
  type Y {
    Y
  }

  type Z {
    Z
  }
}
",
        "type X

@target(erlang)
type Y {
  Y
}

@target(erlang)
type Z {
  Z
}

@target(javascript)
type Y {
  Y
}

@target(javascript)
type Z {
  Z
}
"
    );
}

#[test]
fn multiple() {
    assert_format!(
        "type X

@target(erlang)
type Y {
  Y
}

@target(javascript)
type Z {
  Z
}
"
    );
}
