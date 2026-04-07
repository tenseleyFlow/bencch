! Multi-value list-directed READ.
! Writes three values on one line, reads them back in one READ.
! Exercises: per-unit token buffer for multi-value lines.
!
! CHECK: 10
! CHECK: 20
! CHECK: 30
program test_io_multi_value_read
    implicit none
    integer :: a, b, c

    open(10, file='/tmp/afs_multi.dat', status='replace')
    write(10, *) 10, 20, 30
    close(10)

    a = 0
    b = 0
    c = 0
    open(10, file='/tmp/afs_multi.dat', status='old')
    read(10, *) a, b, c
    close(10)

    print *, a
    print *, b
    print *, c
end program
