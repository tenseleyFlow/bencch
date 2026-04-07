! CHECK: 25
program test_nested
    implicit none
    integer :: i, j, total
    total = 0
    do i = 1, 5
        do j = 1, 5
            total = total + 1
        end do
    end do
    print *, total
end program
