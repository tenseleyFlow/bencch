! Audit #4 MAJOR-4 — PARAMETER-attributed locals are now inlined
! at every use site instead of being SAVE-promoted to .data.
!
! Fixed: alloc_decls now detects Attribute::Parameter (or
! standalone PARAMETER stmts) on a scalar local. If the
! initializer const-folds, the value is stored on LocalInfo's
! new `inline_const` field and `Expr::Name` lowering
! materializes the constant directly via b.const_i32/i64/f32/f64
! at every use, with no .data slot allocated for the parameter.
!
! IR-shape assertions (audit5 MIN-2):
!   * The PARAMETER must NOT get a SAVE-style global. Without
!     inline_const, alloc_decls would emit `global @afs_save_*_k`.
!   * The PARAMETER must NOT have a `load` instruction reading
!     from its sentinel slot — every use site materializes a
!     fresh `const_int 10`.
!   * `result = k * k` must lower to two const_int 10s feeding
!     an `imul` directly.
!
! CHECK: 100
! IR_NOT: global @afs_save
! IR_NOT: load %0 : i32
! IR_CHECK: const_int 10
! IR_CHECK: const_int 10
! IR_CHECK: imul
program audit4_maj4_parameter_inlined
  call s()
contains
  subroutine s()
    integer, parameter :: k = 10
    integer :: result
    result = k * k
    print *, result
  end subroutine
end program
