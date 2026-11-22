//! Tokens to a syntax tree, for the subset of Cypher ProximaDB answers:
//!
//! ```text
//! query     MATCH pattern [WHERE condition] RETURN item {, item} [LIMIT number]
//! pattern   [name =] (shortestPath( chain ) | chain)
//! chain     node [link node]
//! node      ( [name] [:Label] [{ property: value }] )
//! link      -[ [name] [:TYPE {| TYPE}] [*[min]..max] [(r, n | condition)] ]->   (or <-[...]-, or -[...]-)
//! condition [NOT] name.property (= | <> | < | <= | > | >=) value | name.property IS [NOT] NULL,
//!           combined with AND, OR, NOT and parentheses
//! item      name | count([DISTINCT] name)
//! TYPE      name {. name}: an intermediary object type gives NAME.a and NAME.b
//! value     'text' | number | true | false | $parameter
//! ```

use crate::core::Direction;
use crate::query::condition::Op;

use super::CypherError;
use super::lexer::{Spanned, Token};

/// How deep parentheses and NOT may nest in a condition: each level is a few recursive calls.
const MAX_NESTING: usize = 64;


#[derive(Debug)]
pub struct Query {
    pub path_name: Option<String>,
    pub shortest: bool,
    pub start: NodePattern,
    pub step: Option<(LinkPattern, NodePattern)>,
    pub condition: Option<Expr>,
    pub returns: Vec<ReturnItem>,
    pub limit: Option<u64>,
}


#[derive(Debug)]
pub struct NodePattern {
    pub name: Option<String>,
    pub label: Option<String>,
    pub properties: Vec<(String, Literal)>,
}


#[derive(Debug)]
pub struct LinkPattern {
    pub name: Option<String>,
    pub types: Vec<String>,
    pub direction: Direction,
    /// `None` for a single link; `(min, max)` for `*min..max`, where a missing max is `None`.
    pub length: Option<(u32, Option<u32>)>,
    /// Memgraph's inline filter `(r, n | condition)`: the name given to each node stepped onto, and its condition.
    pub filter: Option<(String, Expr)>,
}


#[derive(Debug)]
pub enum Expr {
    Compare {
        name: String,
        property: String,
        op: Op,
        value: Literal,
    },
    IsNull {
        name: String,
        property: String,
        negated: bool,
    },
    /// `b <> a`: two nodes of the pattern are different nodes.
    Different {
        left: String,
        right: String,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}


#[derive(Clone, Debug)]
pub enum Literal {
    Str(String),
    Num(f64),
    Bool(bool),
    Param(String),
}


#[derive(Debug)]
pub enum ReturnItem {
    Name(String),
    Count { name: String },
}


pub fn parse(tokens: Vec<Spanned>) -> Result<Query, CypherError> {
    let mut parser = Parser {
        tokens,
        at: 0,
        nesting: 0,
    };

    parser.query()
}


struct Parser {
    tokens: Vec<Spanned>,
    at: usize,
    nesting: usize,
}


impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.at].0
    }


    fn offset(&self) -> usize {
        self.tokens[self.at].1
    }


    fn advance(&mut self) -> Token {
        let token = self.tokens[self.at].0.clone();

        if token != Token::End {
            self.at += 1;
        }

        token
    }


    fn error<T>(&self, message: impl Into<String>) -> Result<T, CypherError> {
        Err(CypherError::at(self.offset(), message))
    }


    fn expect(&mut self, wanted: Token, what: &str) -> Result<(), CypherError> {
        if *self.peek() == wanted {
            self.advance();
            return Ok(());
        }

        self.error(format!("expected {what}"))
    }


    fn at_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Token::Ident(word) if word.eq_ignore_ascii_case(keyword))
    }


    fn keyword(&mut self, keyword: &str) -> Result<(), CypherError> {
        if self.at_keyword(keyword) {
            self.advance();
            return Ok(());
        }

        self.error(format!("expected {keyword}"))
    }


    fn name(&mut self, what: &str) -> Result<String, CypherError> {
        match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                Ok(name)
            }

            _ => self.error(format!("expected {what}")),
        }
    }


    fn number(&mut self, what: &str) -> Result<u64, CypherError> {
        match *self.peek() {
            Token::Num(n) if n >= 0.0 && n.fract() == 0.0 => {
                self.advance();
                Ok(n as u64)
            }

            _ => self.error(format!("expected {what}")),
        }
    }


    // ---------------------------------------------------------------- query


    fn query(&mut self) -> Result<Query, CypherError> {
        self.keyword("MATCH")?;

        let (path_name, shortest, start, step) = self.pattern()?;

        let condition = if self.at_keyword("WHERE") {
            self.advance();
            Some(self.expr()?)
        } else {
            None
        };

        self.keyword("RETURN")?;

        let mut returns = vec![self.return_item()?];

        while *self.peek() == Token::Comma {
            self.advance();
            returns.push(self.return_item()?);
        }

        let limit = if self.at_keyword("LIMIT") {
            self.advance();
            Some(self.number("a number after LIMIT")?)
        } else {
            None
        };

        if *self.peek() != Token::End {
            return self
                .error("expected the end of the query (ProximaDB answers one MATCH … RETURN)");
        }

        Ok(Query {
            path_name,
            shortest,
            start,
            step,
            condition,
            returns,
            limit,
        })
    }


    #[allow(clippy::type_complexity)]
    fn pattern(
        &mut self,
    ) -> Result<
        (
            Option<String>,
            bool,
            NodePattern,
            Option<(LinkPattern, NodePattern)>,
        ),
        CypherError,
    > {
        let after = self.tokens.get(self.at + 1).map(|(token, _)| token);

        let path_name = match (self.peek().clone(), after) {
            (Token::Ident(name), Some(Token::Eq)) => {
                self.advance();
                self.advance();
                Some(name)
            }

            _ => None,
        };

        let shortest = self.at_keyword("shortestPath");

        if shortest {
            self.advance();
            self.expect(Token::LParen, "( after shortestPath")?;
        }

        let start = self.node()?;

        let step = match self.peek() {
            Token::Dash | Token::LeftArrow => {
                let link = self.link()?;
                let end = self.node()?;

                Some((link, end))
            }

            _ => None,
        };

        if shortest {
            self.expect(Token::RParen, ") closing shortestPath")?;
        }

        if *self.peek() == Token::Comma {
            return self.error(
                "ProximaDB answers one pattern: a start node, optionally a link and an end node",
            );
        }

        Ok((path_name, shortest, start, step))
    }


    fn node(&mut self) -> Result<NodePattern, CypherError> {
        self.expect(Token::LParen, "( starting a node")?;

        let name = match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                Some(name)
            }

            _ => None,
        };

        let label = if *self.peek() == Token::Colon {
            self.advance();
            Some(self.name("a node label after :")?)
        } else {
            None
        };

        let mut properties = Vec::new();

        if *self.peek() == Token::LBrace {
            self.advance();

            loop {
                let key = self.name("a property name")?;
                self.expect(Token::Colon, ": after the property name")?;
                let value = self.literal()?;

                properties.push((key, value));

                if *self.peek() != Token::Comma {
                    break;
                }

                self.advance();
            }

            self.expect(Token::RBrace, "} closing the properties")?;
        }

        self.expect(Token::RParen, ") closing the node")?;

        Ok(NodePattern {
            name,
            label,
            properties,
        })
    }


    fn link(&mut self) -> Result<LinkPattern, CypherError> {
        let points_back = match self.advance() {
            Token::LeftArrow => true,
            Token::Dash => false,
            _ => return self.error("expected - or <- starting a link"),
        };

        self.expect(Token::LBracket, "[ starting the link")?;

        let name = match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                Some(name)
            }

            _ => None,
        };

        let mut types = Vec::new();

        if *self.peek() == Token::Colon {
            self.advance();
            types.push(self.link_type_name("a link type after :")?);

            while *self.peek() == Token::Pipe {
                self.advance();

                if *self.peek() == Token::Colon {
                    self.advance();
                }

                types.push(self.link_type_name("a link type after |")?);
            }
        }

        let length = if *self.peek() == Token::Star {
            self.advance();
            Some(self.length()?)
        } else {
            None
        };

        let filter = if *self.peek() == Token::LParen {
            Some(self.inline_filter()?)
        } else {
            None
        };

        self.expect(Token::RBracket, "] closing the link")?;

        let points_forward = match self.advance() {
            Token::Arrow => true,
            Token::Dash => false,
            _ => return self.error("expected - or -> ending the link"),
        };

        let direction = match (points_back, points_forward) {
            (false, true) => Direction::Forward,
            (true, false) => Direction::Backward,
            (false, false) => Direction::Both,
            (true, true) => return self.error("a link can't point both ways"),
        };

        Ok(LinkPattern {
            name,
            types,
            direction,
            length,
            filter,
        })
    }


    /// A link type's name, which may contain dots: an intermediary object type's link types are `NAME.a` and
    /// `NAME.b`.
    fn link_type_name(&mut self, what: &str) -> Result<String, CypherError> {
        let mut name = self.name(what)?;

        while *self.peek() == Token::Dot {
            self.advance();
            name.push('.');
            name.push_str(&self.name("the rest of the link type's name after .")?);
        }

        Ok(name)
    }


    /// After `*`: `[min]..max`, `min..`, `n` (exactly n), or nothing.
    fn length(&mut self) -> Result<(u32, Option<u32>), CypherError> {
        let min = match self.peek() {
            Token::Num(_) => Some(self.number("a length")?.min(u32::MAX as u64) as u32),
            _ => None,
        };

        if *self.peek() != Token::DotDot {
            return Ok(match min {
                Some(n) => (n, Some(n)),
                None => (1, None),
            });
        }

        self.advance();

        let max = match self.peek() {
            Token::Num(_) => Some(self.number("a length")?.min(u32::MAX as u64) as u32),
            _ => None,
        };

        Ok((min.unwrap_or(1), max))
    }


    /// `(r, n | condition)`
    fn inline_filter(&mut self) -> Result<(String, Expr), CypherError> {
        self.expect(Token::LParen, "(")?;
        self.name("a name for the link")?;
        self.expect(Token::Comma, ", between the link's and the node's names")?;

        let node = self.name("a name for the node")?;

        self.expect(Token::Pipe, "| before the filter's condition")?;

        let condition = self.expr()?;

        self.expect(Token::RParen, ") closing the filter")?;

        Ok((node, condition))
    }


    fn return_item(&mut self) -> Result<ReturnItem, CypherError> {
        if self.at_keyword("count") {
            self.advance();
            self.expect(Token::LParen, "( after count")?;

            if self.at_keyword("DISTINCT") {
                self.advance();
            }

            let name = self.name("a name to count")?;

            self.expect(Token::RParen, ") closing count")?;

            return Ok(ReturnItem::Count { name });
        }

        let name = self.name("a name to return")?;

        if *self.peek() == Token::Dot {
            return self.error(
                "return the node itself: ProximaDB returns nodes and links, and their properties come from Quarry",
            );
        }

        Ok(ReturnItem::Name(name))
    }


    // ---------------------------------------------------------------- conditions


    fn expr(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.and()?;

        while self.at_keyword("OR") {
            self.advance();
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }

        Ok(left)
    }


    fn and(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.not()?;

        while self.at_keyword("AND") {
            self.advance();
            left = Expr::And(Box::new(left), Box::new(self.not()?));
        }

        Ok(left)
    }


    fn not(&mut self) -> Result<Expr, CypherError> {
        if self.at_keyword("NOT") {
            self.advance();

            let inner = self.nested(Self::not)?;

            return Ok(Expr::Not(Box::new(inner)));
        }

        self.atom()
    }


    fn nested(
        &mut self,
        parse: fn(&mut Self) -> Result<Expr, CypherError>,
    ) -> Result<Expr, CypherError> {
        if self.nesting == MAX_NESTING {
            return self.error(format!("a condition nests at most {MAX_NESTING} deep"));
        }

        self.nesting += 1;
        let inner = parse(self);
        self.nesting -= 1;

        inner
    }


    fn atom(&mut self) -> Result<Expr, CypherError> {
        if *self.peek() == Token::LParen {
            self.advance();
            let inner = self.nested(Self::expr)?;
            self.expect(Token::RParen, ")")?;

            return Ok(inner);
        }

        let name = self.name("a condition such as n.status = 'active'")?;

        // `b <> a`, comparing two nodes, not a property with a value.
        if *self.peek() == Token::Ne {
            self.advance();

            let right = self.name("the name of the other node, as in b <> a")?;

            return Ok(Expr::Different { left: name, right });
        }

        self.expect(Token::Dot, ". between the name and the property")?;
        let property = self.name("a property name")?;

        if self.at_keyword("IS") {
            self.advance();

            let negated = self.at_keyword("NOT");

            if negated {
                self.advance();
            }

            self.keyword("NULL")?;

            return Ok(Expr::IsNull {
                name,
                property,
                negated,
            });
        }

        let op = match self.advance() {
            Token::Eq => Op::Eq,
            Token::Ne => Op::Ne,
            Token::Lt => Op::Lt,
            Token::Le => Op::Le,
            Token::Gt => Op::Gt,
            Token::Ge => Op::Ge,
            _ => return self.error("expected a comparison: = <> < <= > >="),
        };

        let value = self.literal()?;

        Ok(Expr::Compare {
            name,
            property,
            op,
            value,
        })
    }


    fn literal(&mut self) -> Result<Literal, CypherError> {
        match self.advance() {
            Token::Str(text) => Ok(Literal::Str(text)),
            Token::Num(n) => Ok(Literal::Num(n)),
            Token::Param(name) => Ok(Literal::Param(name)),

            Token::Dash => match self.advance() {
                Token::Num(n) => Ok(Literal::Num(-n)),
                _ => self.error("expected a number after -"),
            },

            Token::Ident(word) if word.eq_ignore_ascii_case("true") => Ok(Literal::Bool(true)),
            Token::Ident(word) if word.eq_ignore_ascii_case("false") => Ok(Literal::Bool(false)),

            _ => self.error("expected a value: 'text', a number, true, false or a $parameter"),
        }
    }
}
