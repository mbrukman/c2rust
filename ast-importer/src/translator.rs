
use syntax::ast::*;
use renamer::Renamer;
use convert_type::TypeConverter;
use idiomize::ast_manip::make_ast::*;
use clang_ast::*;
use syntax::ptr::*;
use syntax::print::pprust::*;
use std::collections::HashSet;

pub struct Translation {
    pub items: Vec<P<Item>>,
    pub type_converter: TypeConverter,
    pub ast_context: AstContext,
    renamer: Renamer<String>,
}

pub struct WithStmts<T> {
    stmts: Vec<Stmt>,
    val: T,
}

impl<T> WithStmts<T> {
    pub fn new(val: T) -> Self {
        WithStmts { stmts: vec![], val, }
    }
    pub fn and_then<U,F: FnOnce(T) -> WithStmts<U>>(self, f : F) -> WithStmts<U> {
        let mut next = f(self.val);
        let mut stmts = self.stmts;
        stmts.append(&mut next.stmts);
        WithStmts {
            val: next.val,
            stmts
        }
    }
    pub fn map<U,F: FnOnce(T) -> U>(self, f : F) -> WithStmts<U> {
        WithStmts {
            val: f(self.val),
            stmts: self.stmts,
        }
    }
}

impl WithStmts<P<Expr>> {
    pub fn to_expr(mut self) -> P<Expr> {
        if self.stmts.is_empty() {
            self.val
        } else {
            self.stmts.push(mk().expr_stmt(self.val));
            mk().block_expr(mk().block(self.stmts))
        }
    }
}

pub fn stmts_block(stmts: Vec<Stmt>) -> P<Block> {
    if stmts.len() == 1 {
        if let StmtKind::Expr(ref e) = stmts[0].node {
            if let ExprKind::Block(ref b) = e.node {
                    return b.clone()
            }
        }
    }
    mk().block(stmts)
}

pub fn with_stmts_opt<T>(opt: Option<WithStmts<T>>) -> WithStmts<Option<T>> {
    match opt {
        None => WithStmts::new(None),
        Some(x) => WithStmts { stmts: x.stmts, val: Some(x.val) },
    }
}

pub fn translate(ast_context: AstContext) -> String {
    use clang_ast::*;
    let mut t = Translation::new(ast_context.clone());

    // Populate renamer with top-level names
    for top_id in ast_context.top_nodes.to_owned() {
        if let Some(x) = ast_context.ast_nodes.get(&top_id) {
           if let Some(y) = x.get_decl_name() {
               t.renamer.insert(y.to_owned(), &y);
           }
        }
    }

    for top_id in ast_context.top_nodes.to_owned() {
        let x = match ast_context.ast_nodes.get(&top_id) {
            Some(n) => n.clone(),
            None => continue,
        };

        if x.tag == ASTEntryTag::TagFunctionDecl {

            let name = expect_string(&x.extras[0]).expect("Expected a name");

            let ty = ast_context.get_type(x.type_id.expect("Expected a type")).expect("Expected a number");
            let funtys = expect_array(&ty.extras[0]).expect("Function declaration type expected");
            let ret = expect_u64(&funtys[0]).expect("Expected a return type");

            let args_n = x.children.len() - 1;
            let args : Vec<(String,u64)> =
                x.children[0 .. args_n]
                 .iter().map(|x| {
                     let p = ast_context.ast_nodes.get(&x.expect("Missing parameter id")).expect("Bad parameter id");
                     let param_name = expect_string(&p.extras[0]).expect("Parameter name required");
                     (param_name, p.type_id.expect("Parameter type required"))
                 }).collect();

            let args : Vec<(&str, u64)> = args.iter().map(|&(ref x,y)| (x.as_str(),y)).collect();
            let body = x.children[args_n].expect("Expected body id");

            t.add_function(&name, &args, ret, body);
        }
    }

    to_string(|s| {

        for x in t.items.iter() {
            s.print_item(x)?
        }

        Ok(())
    })
}

/// Convert a boolean expression to a c_int
fn bool_to_int(val: P<Expr>) -> P<Expr> {
    mk().cast_expr(val, mk().path_ty(vec!["libc","c_int"]))
}

/// Convert a boolean expression to a c_int
fn int_to_bool(val: P<Expr>) -> P<Expr> {
    let zero = mk().lit_expr(mk().int_lit(0, LitIntType::Unsuffixed));
    mk().binary_expr(mk().spanned(BinOpKind::Ne), zero, val)
}

impl Translation {
    pub fn new(ast_context: AstContext) -> Translation {
        Translation {
            items: vec![],
            type_converter: TypeConverter::new(),
            ast_context,
            renamer: Renamer::new(HashSet::new()),
            // XXX: Populate reserved words
        }
    }

    pub fn add_struct(&mut self, name: Ident, fields: &[(&str, u64)]) {
        let struct_fields =
            fields
                .iter()
                .map(|&(id, ty)| {
                    let ty = self.type_converter.convert(&self.ast_context, ty);
                    mk().struct_field(id, ty)
                })
                .collect();

        let item = mk().struct_item(name, struct_fields);

        self.items.push(item);
    }

    pub fn add_typedef(&mut self, name: &str, typeid: u64) {
        let ty = self.convert_type(typeid);
        let item = mk().type_item(name, ty);
        self.items.push(item);
    }

    pub fn add_function(&mut self, name: &str, arguments: &[(&str, u64)], return_type: u64, body: u64) {
        // Start scope for function parameters
        self.renamer.add_scope();

        let args: Vec<Arg> = arguments.iter().map(|&(var, ty)| {
            let rust_var = self.renamer.insert(var.to_string(), var).expect("Failed to insert argument");
            mk().arg(self.convert_type(ty), mk().ident_pat(rust_var))
        }).collect();

        let ret = FunctionRetTy::Ty(self.convert_type(return_type));

        let decl = mk().fn_decl(args, ret);

        let block = self.convert_function_body(body);

        // End scope for function parameters
        self.renamer.drop_scope();

        self.items.push(mk().fn_item(name, decl, block));
    }

    fn convert_function_body(&mut self, body_id: u64) -> P<Block> {
        let node =
            self.ast_context
                .ast_nodes
                .get(&body_id)
                .expect("Expected function body node")
                .to_owned(); // release immutable borrow on self

        assert_eq!(node.tag, ASTEntryTag::TagCompoundStmt);

        // Open function body scope
        self.renamer.add_scope();

        let stmts: Vec<Stmt> =
            node.children
                .iter()
                .flat_map(|&stmt_id| {
                    self.convert_stmt(stmt_id.unwrap())
                }).collect();

        // Close function body scope
        self.renamer.drop_scope();

        stmts_block(stmts)
    }

    fn convert_stmt(&mut self, stmt_id: u64) -> Vec<Stmt> {
        let node: AstNode =
            self.ast_context
                .ast_nodes
                .get(&stmt_id)
                .unwrap()
                .to_owned(); // release immutable borrow on self

        match node.tag {
            ASTEntryTag::TagDeclStmt =>
                node.children.iter().flat_map(|decl_id| self.convert_decl_stmt(decl_id.unwrap())).collect(),
            ASTEntryTag::TagReturnStmt => {
                self.convert_return_stmt(node.children[0])
            }
            ASTEntryTag::TagIfStmt => {
                self.convert_if_stmt(node.children[0].unwrap(), node.children[1].unwrap(), node.children[2])
            }
            ASTEntryTag::TagWhileStmt => {
                self.convert_while_stmt(node.children[0].unwrap(), node.children[1].unwrap())
            }
            ASTEntryTag::TagNullStmt => {
                vec![]
            }
            ASTEntryTag::TagCompoundStmt => {
                self.renamer.add_scope();

                let stmts = node.children.into_iter().flat_map(|x| x).flat_map(|x| self.convert_stmt(x)).collect();

                self.renamer.drop_scope();

                vec![mk().expr_stmt(mk().block_expr(stmts_block(stmts)))]
            }
            t => {
                let mut xs = self.convert_expr(stmt_id);
                xs.stmts.push(mk().expr_stmt(xs.val));
                xs.stmts
            },
        }
    }

    fn convert_while_stmt(&mut self, cond_id: u64, body_id: u64) -> Vec<Stmt> {

        let cond = self.convert_expr(cond_id);
        let body = self.convert_stmt(body_id);

        let rust_cond = cond.to_expr();
        let rust_body = stmts_block(body);

        vec![mk().expr_stmt(mk().while_expr(rust_cond, rust_body))]
    }

    fn convert_if_stmt(&mut self, cond_id: u64, then_id: u64, else_id: Option<u64>) -> Vec<Stmt> {
        let mut cond = self.convert_expr(cond_id);
        let then_stmts = stmts_block(self.convert_stmt(then_id));
        let else_stmts = else_id.map(|x| { mk().block_expr(stmts_block(self.convert_stmt(x)))});

        cond.stmts.push(mk().expr_stmt(mk().ifte_expr(cond.val, then_stmts, else_stmts)));
        cond.stmts
    }

    fn convert_return_stmt(&mut self, result_id: Option<u64>) -> Vec<Stmt> {
        let val = result_id.map(|i| self.convert_expr(i));
        let mut ws = with_stmts_opt(val);
        let ret = mk().expr_stmt(mk().return_expr(ws.val));

        ws.stmts.push(ret);
        ws.stmts
    }

    fn convert_decl_stmt(&mut self, decl_id: u64) -> Vec<Stmt> {
        let node: AstNode =
            self.ast_context
                .ast_nodes
                .get(&decl_id)
                .unwrap()
                .to_owned(); // release immutable borrow on self

        match node.tag {
            ASTEntryTag::TagVarDecl => {
                let var_name = expect_string(&node.extras[0]).unwrap();
                let rust_name = self.renamer.insert(var_name.clone(), &var_name).unwrap();
                let pat = mk().set_mutbl(Mutability::Mutable).ident_pat(rust_name);
                let init = with_stmts_opt(node.children[0].map(|x| self.convert_expr(x)));
                let ty = self.convert_type(node.type_id.unwrap());
                let local = mk().local(pat, Some(ty), init.val);

                let mut stmts = init.stmts;
                stmts.push(mk().local_stmt(P(local)));
                stmts
            }
            t => panic!("Declaration not implemented {:?}", t),
        }
    }

    fn convert_type(&self, type_id: u64) -> P<Ty> {
        self.type_converter.convert(&self.ast_context, type_id)
    }

    fn convert_expr(&mut self, expr_id: u64) -> WithStmts<P<Expr>> {
        let node = self.ast_context.ast_nodes.get(&expr_id).expect("Expected expression node").clone();
        self.convert_expr_node(node)

    }
    fn convert_expr_node(&mut self, node: AstNode) -> WithStmts<P<Expr>> {
        match node.tag {
            ASTEntryTag::TagDeclRefExpr =>
                {
                    let child =
                        self.ast_context.ast_nodes.get(&node.children[0].expect("Expected decl id"))
                            .expect("Expected decl node");

                    let varname = child.get_decl_name().expect("expected variable name").to_owned();
                    let rustname = self.renamer.get(varname).expect("name not declared");
                    WithStmts::new(mk().path_expr(vec![rustname]))
                }
            ASTEntryTag::TagIntegerLiteral =>
                {
                    let val = expect_u64(&node.extras[0]).expect("Expected value");
                    let _ty = self.convert_type(node.type_id.expect("Expected type"));
                    WithStmts::new(mk().lit_expr(mk().int_lit(val.into(), LitIntType::Unsuffixed)))
                }
            ASTEntryTag::TagCharacterLiteral =>
                {
                    let val = expect_u64(&node.extras[0]).expect("Expected value");
                    let _ty = self.convert_type(node.type_id.expect("Expected type"));
                    WithStmts::new(mk().lit_expr(mk().int_lit(val.into(), LitIntType::Unsuffixed)))
                }
            ASTEntryTag::TagFloatingLiteral =>
                {
                    let val = expect_f64(&node.extras[0]).expect("Expected value");
                    let str = format!("{}", val);
                    WithStmts::new(mk().lit_expr(mk().float_unsuffixed_lit(str)))
                }
            ASTEntryTag::TagImplicitCastExpr =>
                {
                    // TODO actually cast
                    // Numeric casts with 'as', pointer casts with transmute
                    let child = node.children[0].expect("Expected subvalue");
                    self.convert_expr(child)
                }
            ASTEntryTag::TagUnaryOperator =>
                {
                    let name = expect_string(&node.extras[0]).expect("Missing binary operator name");
                    let mut arg = self.convert_expr(node.children[0].expect("Missing value"));
                    let type_id = node.type_id.unwrap();
                    let cty = self.ast_context.get_type(type_id).unwrap();
                    let ty = self.convert_type(type_id);
                    let mut unary = self.convert_unary_operator(&name, cty, ty, arg.val);
                    arg.stmts.append(&mut unary.stmts);
                    WithStmts {
                        stmts: arg.stmts,
                        val: unary.val,
                    }
                }
            ASTEntryTag::TagBinaryOperator =>
                {
                    let name = expect_string(&node.extras[0]).expect("Missing binary operator name");
                    let lhs_node = self.ast_context.ast_nodes.get(&node.children[0].expect("lhs id")).expect("lhs node").to_owned();
                    let lhs_ty = self.ast_context.get_type(lhs_node.type_id.expect("lhs ty id")).expect("lhs ty");
                    let lhs = self.convert_expr_node(lhs_node);
                    let rhs_node = self.ast_context.ast_nodes.get(&node.children[1].expect("rhs id")).expect("rhs node").to_owned();
                    let rhs_ty = self.ast_context.get_type(rhs_node.type_id.expect("rhs ty id")).expect("rhs ty");
                    let rhs = self.convert_expr_node(rhs_node);
                    let type_id = node.type_id.unwrap();
                    let cty = self.ast_context.get_type(type_id).unwrap();
                    let ty = self.convert_type(type_id);
                    let bin =
                        self.convert_binary_operator(&name, ty, cty, lhs_ty, rhs_ty, lhs.val, rhs.val);

                    WithStmts {
                        stmts: lhs.stmts.into_iter().chain(rhs.stmts).chain(bin.stmts).collect(),
                        val: bin.val,
                    }
                },
            ASTEntryTag::TagCallExpr =>
                {
                    let mut stmts = vec![];
                    let mut exprs = vec![];

                    for x in node.children.iter() {
                        let mut res = self.convert_expr(x.unwrap());
                        stmts.append(&mut res.stmts);
                        exprs.push(res.val);
                    }

                    let fun = exprs.remove(0);

                    WithStmts {
                        stmts,
                        val: mk().call_expr(fun, exprs),
                    }
                }
            ASTEntryTag::TagMemberExpr => {
                let mut struct_val = self.convert_expr(node.children[0].expect("Missing structval"));
                let field_node = self.ast_context.ast_nodes.get(&node.children[1].expect("Missing structfield id")).expect("Missing structfield").clone();
                let field_name = expect_str(&field_node.extras[0]).expect("expected field name");

                struct_val.val = mk().field_expr(struct_val.val, field_name);
                struct_val
            }
            t => panic!("Expression not implemented {:?}", t),
        }
    }

    pub fn convert_unary_operator(&mut self, name: &str, ctype: TypeNode, ty: P<Ty>, arg: P<Expr>) -> WithStmts<P<Expr>> {
        match name {
            "&" => {
                let addr_of_arg = mk().set_mutbl(Mutability::Mutable).addr_of_expr(arg);
                let ptr = mk().cast_expr(addr_of_arg, ty);
                WithStmts::new(ptr)
            },
            n => panic!("unary operator {} not implemented", n),
        }
    }

    pub fn convert_binary_operator(&mut self, name: &str, ty: P<Ty>, ctype: TypeNode, lhs_type: TypeNode, rhs_type: TypeNode, lhs: P<Expr>, rhs: P<Expr>) -> WithStmts<P<Expr>>
    {
        match name {

            "+" => WithStmts::new(self.convert_addition(lhs_type, rhs_type, lhs, rhs)),
            "-" => WithStmts::new(self.convert_subtraction(lhs_type, rhs_type, lhs, rhs)),

            "*" if ctype.is_unsigned_integral_type() =>
                WithStmts::new(mk().method_call_expr(lhs, mk().path_segment("wrapping_mul"), vec![rhs])),
            "*" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Mul), lhs, rhs)),

            "/" if ctype.is_unsigned_integral_type() =>
                WithStmts::new(mk().method_call_expr(lhs, mk().path_segment("wrapping_div"), vec![rhs])),
            "/" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Div), lhs, rhs)),

            "%" if ctype.is_unsigned_integral_type() =>
                WithStmts::new(mk().method_call_expr(lhs, mk().path_segment("wrapping_rem"), vec![rhs])),
            "%" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Rem), lhs, rhs)),

            "^" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::BitXor), lhs, rhs)),

            ">>" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Shr), lhs, rhs)),

            "==" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Eq),
                                                        lhs, rhs)).map(bool_to_int),
            "!=" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Ne), lhs, rhs)).map(bool_to_int),
            "<" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Lt), lhs, rhs)).map(bool_to_int),
            ">" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Gt), lhs, rhs)).map(bool_to_int),
            ">=" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Ge), lhs, rhs)).map(bool_to_int),
            "<=" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Le), lhs, rhs)).map(bool_to_int),

            "&&" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::And), lhs, rhs)),
            "||" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::Or), lhs, rhs)),

            "&" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::BitAnd), lhs, rhs)),
            "|" => WithStmts::new(mk().binary_expr(mk().spanned(BinOpKind::BitOr), lhs, rhs)),

            "+="  => self.convert_binary_assignment("+",  ty, ctype, lhs_type, rhs_type, lhs, rhs),
            "-="  => self.convert_binary_assignment("-",  ty, ctype, lhs_type, rhs_type, lhs, rhs),
            "*="  => self.convert_binary_assignment("*",  ty, ctype, lhs_type, rhs_type, lhs, rhs),
            "/="  => self.convert_binary_assignment("/",  ty, ctype, lhs_type, rhs_type ,lhs, rhs),
            "%="  => self.convert_binary_assignment("%",  ty, ctype, lhs_type, rhs_type ,lhs, rhs),
            "^="  => self.convert_binary_assignment("^",  ty, ctype, lhs_type, rhs_type ,lhs, rhs),
            "<<=" => self.convert_binary_assignment("<<", ty, ctype, lhs_type, rhs_type ,lhs, rhs),
            ">>=" => self.convert_binary_assignment(">>", ty, ctype, lhs_type, rhs_type ,lhs, rhs),
            "|="  => self.convert_binary_assignment("|",  ty, ctype, lhs_type, rhs_type ,lhs, rhs),
            "&="  => self.convert_binary_assignment("&",  ty, ctype, lhs_type, rhs_type ,lhs, rhs),

            "=" => self.convert_assignment(lhs, rhs),

            op => panic!("Unknown binary operator {}", op),
        }
    }

    fn convert_binary_assignment(&mut self, name: &str, ty: P<Ty>, ctype: TypeNode, lhs_type: TypeNode, rhs_type: TypeNode, lhs: P<Expr>, rhs: P<Expr>) -> WithStmts<P<Expr>> {
        // Improvements:
        // * Don't create fresh names in place of lhs that is already a name
        // * Don't create block, use += for a statement
        let ptr_name = self.renamer.fresh();
        // let ref mut p = lhs;
        let compute_lhs =
            mk().local_stmt(
                P(mk().local(mk().set_mutbl(Mutability::Mutable).ident_ref_pat(&ptr_name),
                             None as Option<P<Ty>>,
                             Some(lhs)))
            );
        // *p
        let deref_lhs = mk().unary_expr("*", mk().ident_expr(&ptr_name));
        // *p + rhs
        let mut val = self.convert_binary_operator(name, ty, ctype, lhs_type, rhs_type, deref_lhs.clone(), rhs);
        // *p = *p + rhs
        let assign_stmt = mk().assign_expr(&deref_lhs, val.val);

        let mut stmts = vec![compute_lhs];
        stmts.append(&mut val.stmts);
        stmts.push(mk().expr_stmt(assign_stmt));

        WithStmts {
            stmts,
            val: deref_lhs
        }
    }

    fn convert_addition(&mut self, lhs_type: TypeNode, rhs_type: TypeNode, lhs: P<Expr>, rhs: P<Expr>) -> P<Expr> {
        let lhs_type = self.ast_context.resolve_type(lhs_type);
        let rhs_type = self.ast_context.resolve_type(rhs_type);

        if lhs_type.is_pointer() {
            mk().method_call_expr(lhs, "offset", vec![rhs])
        } else if rhs_type.is_pointer() {
            mk().method_call_expr(rhs, "offset", vec![lhs])
        } else if lhs_type.is_unsigned_integral_type() {
            mk().method_call_expr(lhs, mk().path_segment("wrapping_add"), vec![rhs])
        } else {
            mk().binary_expr(mk().spanned(BinOpKind::Add), lhs, rhs)
        }
    }

    fn convert_subtraction(&mut self, lhs_type: TypeNode, rhs_type: TypeNode, lhs: P<Expr>, rhs: P<Expr>) -> P<Expr> {
        let lhs_type = self.ast_context.resolve_type(lhs_type);
        let rhs_type = self.ast_context.resolve_type(rhs_type);

        if rhs_type.is_pointer() {
            mk().method_call_expr(rhs, "offset_to", vec![lhs])
        } else if lhs_type.is_pointer() {
            let neg_rhs = mk().unary_expr(UnOp::Neg, rhs);
            mk().method_call_expr(lhs, "offset", vec![neg_rhs])
        } else if lhs_type.is_unsigned_integral_type() {
            mk().method_call_expr(lhs, mk().path_segment("wrapping_sub"), vec![rhs])
        } else {
            mk().binary_expr(mk().spanned(BinOpKind::Sub), lhs, rhs)
        }
    }

    fn convert_assignment(&mut self, lhs: P<Expr>, rhs: P<Expr>) -> WithStmts<P<Expr>> {
        // Improvements:
        // * Don't create fresh names in place of lhs that is already a name
        // * Don't create block, use += for a statement
        let ptr_name = self.renamer.fresh();
        // let ref mut p = lhs;
        let compute_lhs =
            mk().local_stmt(
                P(mk().local(mk().set_mutbl(Mutability::Mutable).ident_ref_pat(&ptr_name),
                             None as Option<P<Ty>>,
                             Some(lhs)))
            );
        // *p
        let deref_lhs = mk().unary_expr("*", mk().ident_expr(&ptr_name));

        // *p = rhs
        let assign_stmt = mk().expr_stmt(mk().assign_expr(&deref_lhs, rhs));

        WithStmts {
            stmts: vec![assign_stmt],
            val: deref_lhs
        }
    }
}
