! CHECK: 55
program test_allocatable
    implicit none
    integer, allocatable :: a(:)
    integer :: i, s

    allocate(a(10))

    do i = 1, 10
        a(i) = i
    end do

    s = 0
    do i = 1, 10
        s = s + a(i)
    end do

    print *, s

    deallocate(a)
end program
