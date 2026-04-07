! WHERE construct with expression mask and multi-array body.
! Tests element-wise evaluation of mask and body.
!
! CHECK: 0
! CHECK: 0
! CHECK: 3
! CHECK: 4
! CHECK: 5
program test_where
    implicit none
    integer, allocatable :: a(:), b(:)
    integer :: i
    allocate(a(5), b(5))
    do i = 1, 5
        a(i) = i
        b(i) = 0
    end do
    where (a > 2)
        b = a
    end where
    print *, b(1)
    print *, b(2)
    print *, b(3)
    print *, b(4)
    print *, b(5)
    deallocate(a, b)
end program
