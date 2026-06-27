use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        "scientific_calculator",
        "Rust-only numeric calculator. Supports evaluate operation for arithmetic expressions and common math functions. Does not support symbolic calculus.",
        json!({"type":"object","properties":{"expression":{"type":"string"},"operation":{"type":"string","enum":["evaluate"]}},"required":["expression"],"additionalProperties":false}),
        |args| async move { calculate(args) },
    ));
}

fn calculate(args: Value) -> Result<String> {
    let expression = args
        .get("expression")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let operation = args
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("evaluate");
    if operation != "evaluate" {
        bail!("rust-simple calculator only supports operation=evaluate for now");
    }
    if expression.is_empty() || expression.len() > 4000 {
        bail!("expression is empty or too long");
    }
    let value = Parser::new(expression).parse()?;
    Ok(serde_json::to_string_pretty(
        &json!({"success": true, "operation": operation, "expression": expression, "result": value}),
    )?)
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
        }
    }

    fn parse(mut self) -> Result<f64> {
        let value = self.expression()?;
        self.skip_ws();
        if self.pos != self.input.len() {
            bail!("unexpected token at byte {}", self.pos);
        }
        Ok(value)
    }

    fn expression(&mut self) -> Result<f64> {
        let mut value = self.term()?;
        loop {
            self.skip_ws();
            if self.consume(b'+') {
                value += self.term()?;
            } else if self.consume(b'-') {
                value -= self.term()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn term(&mut self) -> Result<f64> {
        let mut value = self.power()?;
        loop {
            self.skip_ws();
            if self.consume(b'*') {
                value *= self.power()?;
            } else if self.consume(b'/') {
                value /= self.power()?;
            } else if self.consume(b'%') {
                value %= self.power()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn power(&mut self) -> Result<f64> {
        let value = self.unary()?;
        self.skip_ws();
        if self.consume(b'^') {
            Ok(value.powf(self.power()?))
        } else {
            Ok(value)
        }
    }

    fn unary(&mut self) -> Result<f64> {
        self.skip_ws();
        if self.consume(b'+') {
            self.unary()
        } else if self.consume(b'-') {
            Ok(-self.unary()?)
        } else {
            self.primary()
        }
    }

    fn primary(&mut self) -> Result<f64> {
        self.skip_ws();
        if self.consume(b'(') {
            let value = self.expression()?;
            self.skip_ws();
            if !self.consume(b')') {
                bail!("expected )");
            }
            return Ok(value);
        }
        if self.peek().is_some_and(|byte| byte.is_ascii_alphabetic()) {
            let ident = self.ident()?;
            self.skip_ws();
            if self.consume(b'(') {
                let arg = self.expression()?;
                self.skip_ws();
                if !self.consume(b')') {
                    bail!("expected ) after function argument");
                }
                return apply_function(&ident, arg);
            }
            return constant(&ident);
        }
        self.number()
    }

    fn number(&mut self) -> Result<f64> {
        self.skip_ws();
        let start = self.pos;
        while self.peek().is_some_and(|byte| {
            byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-')
        }) {
            if matches!(self.peek(), Some(b'+' | b'-')) && self.pos > start {
                let prev = self.input[self.pos - 1];
                if prev != b'e' && prev != b'E' {
                    break;
                }
            }
            self.pos += 1;
        }
        if start == self.pos {
            bail!("expected number at byte {}", self.pos);
        }
        std::str::from_utf8(&self.input[start..self.pos])?
            .parse::<f64>()
            .map_err(Into::into)
    }

    fn ident(&mut self) -> Result<String> {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            self.pos += 1;
        }
        Ok(std::str::from_utf8(&self.input[start..self.pos])?.to_ascii_lowercase())
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }
}

fn apply_function(name: &str, value: f64) -> Result<f64> {
    Ok(match name {
        "sin" => value.sin(),
        "cos" => value.cos(),
        "tan" => value.tan(),
        "asin" => value.asin(),
        "acos" => value.acos(),
        "atan" => value.atan(),
        "sqrt" => value.sqrt(),
        "abs" => value.abs(),
        "ln" => value.ln(),
        "log" | "log10" => value.log10(),
        "exp" => value.exp(),
        "floor" => value.floor(),
        "ceil" => value.ceil(),
        "round" => value.round(),
        _ => bail!("unknown function: {name}"),
    })
}

fn constant(name: &str) -> Result<f64> {
    Ok(match name {
        "pi" => std::f64::consts::PI,
        "e" => std::f64::consts::E,
        "tau" => std::f64::consts::TAU,
        _ => bail!("unknown constant: {name}"),
    })
}
