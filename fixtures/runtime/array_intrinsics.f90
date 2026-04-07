! Array query and reduction intrinsics: SIZE, SUM, MAXVAL, MINVAL.
!
! CHECK: 4
! CHECK: 60
! CHECK: 50
! CHECK: -20
program test_array_intrinsics
    implicit none
    integer, allocatable :: a(:)

    allocate(a(4))
    a(1) = 10
    a(2) = 20
    a(3) = 50
    a(4) = -20

    print *, size(a)
    print *, sum(a)
    print *, maxval(a)
    print *, minval(a)

    deallocate(a)
end program
