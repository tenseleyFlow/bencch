! CHECK: 42
! IR_CHECK: func @mem2reg_ir_comments
! IR_CHECK: call @afs_write_int
! IR_NOT: zeroinit
program mem2reg_ir_comments
    implicit none
    integer :: x

    x = 42
    print *, x
end program mem2reg_ir_comments
